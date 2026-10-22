//! Finding and opening a workspace (§2.5 of the M5 design): by name through the registry, by
//! its directory, or from a directory inside it, upwards to the nearest `.swarmws/`.
//!
//! [`find`] needs no store: the CLI and the hook commands use it to read the store's root a
//! workspace names before they open the store. [`Workspace::open`] then finds it again with the
//! store open, finishes a materialisation a crash left, and settles the identity its mutations
//! act as.
//!
//! The M5 plan's task W1 froze these; task W5 writes [`Workspace::open`]'s body, which until
//! then fails with `E_INVALID_ARG` naming it.

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_store::FsOps;

use crate::Ctx;
use crate::registry::Registry;
use crate::state::{self, WorkspaceFile};

/// Where to look for a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locate {
    /// By its name, through the registry.
    Name(String),
    /// At this directory, its root.
    Path(Utf8PathBuf),
    /// At this directory or the nearest above it that holds a `.swarmws/workspace.json`.
    Cwd(Utf8PathBuf),
}

/// A workspace found: its root and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The workspace's root directory.
    pub dir: Utf8PathBuf,
    /// Its `workspace.json`.
    pub file: WorkspaceFile,
}

/// A workspace opened against its store, which its mutations act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The workspace's root directory.
    pub dir: Utf8PathBuf,
    /// Its `workspace.json`, as it was opened.
    pub file: WorkspaceFile,
    /// The identity its mutations act as: the caller's, else the workspace's `created_by`.
    pub who: Identity,
}

impl Workspace {
    /// The workspace `at` names, of the store `ctx` holds: found, a materialisation a crash
    /// left finished first, its identity `ctx.who` or else its `created_by`. It takes no lock:
    /// each mutation takes the lock.
    pub fn open(_ctx: &Ctx<'_>, _at: Locate) -> Result<Workspace, OpError> {
        Err(OpError::new(
            ErrorCode::InvalidArg,
            "opening a workspace is not built yet (the M5 plan's task W5)",
        ))
    }
}

/// The workspace `at` names, read through `fs`; `registry` resolves a name. No store is needed.
///
/// - [`Locate::Cwd`]: the nearest directory at or above it with a `.swarmws/workspace.json`,
///   by `std::fs` metadata (read-only), up to the root;
/// - [`Locate::Path`]: that directory;
/// - [`Locate::Name`]: the registry entry's directory; `E_INVALID_ARG` without a registry.
///
/// `E_WS_NOT_FOUND` when there is none.
pub fn find(at: &Locate, registry: Option<&Registry>, fs: &dyn FsOps) -> Result<Found, OpError> {
    let dir = match at {
        Locate::Cwd(start) => nearest(start)?,
        Locate::Path(dir) => dir.clone(),
        Locate::Name(name) => entry_dir(name, registry)?,
    };
    let file = state::read(fs, &dir)?;
    Ok(Found { dir, file })
}

/// The nearest directory at or above `start` that holds a `.swarmws/workspace.json`, up to the
/// root. Read-only: `std::fs` metadata, as the untracked walk reads the host.
fn nearest(start: &Utf8Path) -> Result<Utf8PathBuf, OpError> {
    let found = start
        .ancestors()
        .find(|dir| std::fs::metadata(state::path_in(dir)).is_ok_and(|meta| meta.is_file()));
    found.map(Utf8Path::to_path_buf).ok_or_else(|| {
        OpError::new(
            ErrorCode::WsNotFound,
            format!(
                "no workspace at or above {start}: no directory there holds {}/{}",
                state::SWARMWS_DIR,
                state::WORKSPACE_JSON
            ),
        )
    })
}

/// The directory of workspace `name`, from `registry`'s entry.
fn entry_dir(name: &str, registry: Option<&Registry>) -> Result<Utf8PathBuf, OpError> {
    let Some(registry) = registry else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("workspace {name:?} is named, but no store is known to look it up in"),
        ));
    };
    match registry.read(name)? {
        Some(entry) => Ok(Utf8PathBuf::from(entry.path)),
        None => Err(OpError::new(
            ErrorCode::WsNotFound,
            format!("the store has no workspace named {name:?}"),
        )
        .with_details(Map::from_iter([("name".to_owned(), Value::from(name))]))
        .with_hint("swarmops ws list")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use swarmops_store::RealFs;

    use super::*;
    use crate::registry::{ENTRY_FORMAT, Entry, REGISTRY_DIR};
    use crate::state::tests::sample;
    use crate::state::{SWARMWS_DIR, State};

    /// Makes a workspace named `name` at `dir`: its `.swarmws/workspace.json`.
    fn make(dir: &Utf8Path, name: &str) -> WorkspaceFile {
        std::fs::create_dir_all(dir.join(SWARMWS_DIR)).unwrap();
        let file = sample(name, State::Clean);
        state::replace(&RealFs, dir, &file).unwrap();
        file
    }

    /// A lookup from a directory finds the nearest workspace at or above it, an inner one before
    /// the one around it, and none above the outermost, up to the root; a lookup by directory
    /// does not walk; a lookup by name goes through the registry and needs one.
    #[test]
    fn find_walks_up_to_the_nearest_workspace_json_and_stops_at_the_root() {
        // 1. An outer workspace, an inner one below it, and a directory beside both.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let outer = root.join("outer");
        let inner = outer.join("a/inner");
        let deep = inner.join("src/deep");
        let beside = root.join("beside");
        let outer_file = make(&outer, "outer");
        let inner_file = make(&inner, "inner");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(&beside).unwrap();
        let fs = RealFs;
        let found = |at: Locate| find(&at, None, &fs);

        // 2. Upwards from a directory: the nearest one, at it or above it.
        let from_deep = found(Locate::Cwd(deep.clone())).unwrap();
        assert_eq!(
            (from_deep.dir, from_deep.file),
            (inner.clone(), inner_file.clone())
        );
        let from_between = found(Locate::Cwd(outer.join("a"))).unwrap();
        assert_eq!(
            (from_between.dir, from_between.file),
            (outer.clone(), outer_file)
        );
        assert_eq!(found(Locate::Cwd(outer.clone())).unwrap().dir, outer);

        // 3. None above the directory beside them, up to the root: not found.
        let none = found(Locate::Cwd(beside)).unwrap_err();
        assert_eq!(none.code, "E_WS_NOT_FOUND", "{none:?}");

        // 4. By directory: that one only, without walking.
        assert_eq!(found(Locate::Path(inner.clone())).unwrap().file, inner_file);
        let not_one = found(Locate::Path(deep)).unwrap_err();
        assert_eq!(not_one.code, "E_WS_NOT_FOUND", "{not_one:?}");

        // 5. By name: through the registry, which it needs.
        let unnamed = found(Locate::Name("inner".to_owned())).unwrap_err();
        assert_eq!(unnamed.code, "E_INVALID_ARG", "{unnamed:?}");
        let store = root.join("store");
        std::fs::create_dir_all(store.join(REGISTRY_DIR)).unwrap();
        let entry = Entry {
            format: ENTRY_FORMAT,
            name: "inner".to_owned(),
            path: inner.to_string(),
            created_by: "claude-code/inner".to_owned(),
            created_at: 1_792_630_000_000,
            session: None,
        };
        let entry_path = store.join(REGISTRY_DIR).join("inner.json");
        std::fs::write(entry_path, serde_json::to_vec(&entry).unwrap()).unwrap();
        let registry = Registry::open(&store, Arc::new(RealFs));
        let by_name = find(&Locate::Name("inner".to_owned()), Some(&registry), &fs).unwrap();
        assert_eq!((by_name.dir, by_name.file), (inner, inner_file));
        let missing = find(&Locate::Name("gone".to_owned()), Some(&registry), &fs).unwrap_err();
        assert_eq!(missing.code, "E_WS_NOT_FOUND", "{missing:?}");
        assert_eq!(missing.details["name"], "gone");
    }
}
