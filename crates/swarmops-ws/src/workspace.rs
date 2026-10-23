//! Finding and opening a workspace (§2.5 of the M5 design): by name through the registry, by
//! its directory, or from a directory inside it, upwards to the nearest `.swarmws/`.
//!
//! [`find`] needs no store: the CLI and the hook commands use it to read the store's root a
//! workspace names before they open the store. [`Workspace::open`] then finds it again with the
//! store open, finishes a materialisation a crash left, and settles the identity its mutations
//! act as.
//!
//! The M5 plan's task W1 froze these; task W5 wrote [`Workspace::open`]'s body.

use std::path::Path;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, Identity, InfoReq, OpError};
use swarmops_store::FsOps;

use crate::lock::{self, WsLock};
use crate::registry::Registry;
use crate::state::{self, WorkspaceFile};
use crate::{Ctx, materialize, pending};

/// Where to look for a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct Found {
    /// The workspace's root directory.
    pub dir: Utf8PathBuf,
    /// Its `workspace.json`.
    pub file: WorkspaceFile,
}

/// A workspace opened against its store, which its mutations act on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
    ///
    /// A workspace of another store than `ctx`'s is refused (`E_INVALID_ARG`,
    /// `details{workspace_repo_id, store_repo_id}`), so that a stray store cannot take one
    /// store's files. A materialisation a crash left is finished under the lock.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the M5 design's frozen signature takes the location by value"
    )]
    pub fn open(ctx: &Ctx<'_>, at: Locate) -> Result<Workspace, OpError> {
        // 1. Found without the store.
        let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
        let found = find(&at, Some(&registry), ctx.fs.as_ref())?;

        // 2. A materialisation left: finished under the lock, the state read again there.
        if pending::read_intent(ctx.fs.as_ref(), &found.dir)?.is_some() {
            let _held = lock::acquire(&found.dir)?;
            return settle(ctx, found.dir);
        }
        settle(ctx, found.dir)
    }
}

/// The workspace `at` names, opened as [`Workspace::open`] opens it, with its lock held: what
/// every mutation starts from. The state is read once the lock is held, so that it is the one
/// the last mutation left.
pub(crate) fn open_locked(ctx: &Ctx<'_>, at: &Locate) -> Result<(Workspace, WsLock), OpError> {
    let registry = Registry::open(&ctx.store_root, Arc::clone(&ctx.fs));
    let found = find(at, Some(&registry), ctx.fs.as_ref())?;
    let held = lock::acquire(&found.dir)?;
    Ok((settle(ctx, found.dir)?, held))
}

/// The workspace at `dir`: its state read, checked to be of `ctx`'s store, its identity
/// settled, and a materialisation a crash left finished (the caller holds the lock if there is
/// one).
fn settle(ctx: &Ctx<'_>, dir: Utf8PathBuf) -> Result<Workspace, OpError> {
    // 1. The state, of this store.
    let file = state::read(ctx.fs.as_ref(), &dir)?;
    let store_repo_id = ctx.store.repo_info(InfoReq::default())?.repo_id;
    if file.repo_id != store_repo_id {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "workspace {:?} belongs to another store (its repo_id {}, this store's {})",
                file.name, file.repo_id, store_repo_id
            ),
        )
        .with_details(Map::from_iter([
            ("workspace_repo_id".to_owned(), Value::from(file.repo_id)),
            ("store_repo_id".to_owned(), Value::from(store_repo_id)),
        ])));
    }

    // 2. Who acts: the caller, else the workspace's creator.
    let who = match &ctx.who {
        Some(who) => who.clone(),
        None => Identity::parse(&file.created_by)?,
    };

    // 3. A materialisation a crash left, finished first.
    let mut ws = Workspace { dir, file, who };
    materialize::finish(ctx, &mut ws)?;
    Ok(ws)
}

/// The host path `path` as the workspace crate takes one: UTF-8 and absolute, else
/// `E_INVALID_ARG`. The CLI turns its arguments into paths through it; the crate reads no
/// working directory, so a relative path has nothing to be relative to.
pub fn host_path(path: &Path) -> Result<Utf8PathBuf, OpError> {
    // 1. UTF-8, as every path the workspace records is.
    let Some(utf8) = Utf8Path::from_path(path) else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!(
                "the path {} is not UTF-8, which a workspace's paths are",
                path.display()
            ),
        ));
    };

    // 2. Absolute.
    if !utf8.is_absolute() {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("the path {utf8:?} is relative: give an absolute one"),
        ));
    }
    Ok(utf8.to_owned())
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
