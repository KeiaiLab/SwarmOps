//! SwarmOps workspaces (M5): a directory on the host holding a sparse copy of one changeset's
//! tree, plus `.swarmws/`, its private state. An agent edits the files with any tool, builds and
//! tests there, and turns its edits into commits, rebases and lands with one call each.
//!
//! A client-side crate: it depends on `dyn StoreOps`, never on the repository or the store's
//! data (operations spec §7). It borrows the store crate's file primitives, `FsOps`, for its own
//! durable files only, and writes the tree through its own host layer, [`HostFs`].
//!
//! ```text
//!   CLI (`so ws …`, the hooks) ─┐            ┌─▶ StoreOps    the store: commit, land, rebase,
//!   MCP (`ws_*` tools)        ──┴─▶ Ctx ─────┤                read, blob_get, …
//!                                            ├─▶ FsOps       workspace.json, intents, registry
//!                                            └─▶ HostFs      the materialised tree
//!
//!   <dir>/.swarmws/              workspace.json, lock, index.bin, journal/, conflicts.json
//!   <store>/workspaces/<n>.json  the registry entry
//! ```
//!
//! One function per operation, each `(&Ctx, request) -> Result<response, OpError>`:
//! [`create()`], [`status()`], [`diff()`], [`commit()`], [`rebase()`], [`resolve()`],
//! [`land()`], [`materialize()`], [`discard()`], [`list()`], [`info()`] and [`delete()`]. The
//! crate reads no environment variable: what the CLI reads from the environment arrives in
//! [`Ctx`] or as an argument.
//!
//! The M5 plan's task W1 froze these names; its later tasks write the bodies, which until then
//! fail with `E_INVALID_ARG` naming the task.

pub mod after;
pub mod bootstrap;
pub mod check;
pub mod commit;
pub mod create;
pub mod delete;
pub mod diff;
pub mod discard;
pub mod hook;
pub mod hostfs;
pub mod index;
pub mod info;
pub mod land;
pub mod list;
pub mod lock;
pub mod markers;
pub mod materialize;
pub mod pending;
pub mod portable;
pub mod rebase;
pub mod registry;
pub mod resolve;
pub mod run;
pub mod scan;
pub mod state;
pub mod status;
pub mod volume;
pub mod workspace;

use std::io;
use std::sync::Arc;

use camino::Utf8PathBuf;
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, Identity, OpError, StoreOps};
use swarmops_store::FsMethod;

pub use commit::commit;
pub use create::create;
pub use delete::delete;
pub use diff::diff;
pub use discard::discard;
pub use hostfs::{HostFs, HostMeta, RealHost};
pub use info::info;
pub use land::land;
pub use list::list;
pub use materialize::materialize;
pub use rebase::rebase;
pub use resolve::resolve;
pub use status::status;
pub use swarmops_store::{FsOps, RealFs};
pub use workspace::{Found, Locate, Workspace};

/// What every workspace operation runs with: the store, where it is, who acts, and the two file
/// layers. The CLI builds it from `--repo` / `SWARMOPS_REPO` or the workspace's `repo` and the
/// caller's identity, if any; the MCP server from its own store and identity.
pub struct Ctx<'a> {
    /// The store's operations.
    pub store: &'a dyn StoreOps,
    /// The store's root on the host, where the registry is: `StoreOps` carries no path.
    pub store_root: Utf8PathBuf,
    /// The caller's identity, if any; a mutation without one acts as the workspace's
    /// `created_by`.
    pub who: Option<Identity>,
    /// The file primitives of the workspace's durable files.
    pub fs: Arc<dyn FsOps>,
    /// The host layer of the materialised tree.
    pub host: Arc<dyn HostFs>,
}

/// `E_IO` for a call `method` on `file` (a path below the workspace's or the store's root) that
/// failed with `e`: `details.fs_op` and `details.file` name them.
pub(crate) fn io_failed(method: FsMethod, file: &str, e: &io::Error) -> OpError {
    OpError::new(ErrorCode::Io, format!("{method} {file}: {e}")).with_details(Map::from_iter([
        ("fs_op".to_owned(), Value::from(method.as_str())),
        ("file".to_owned(), Value::from(file)),
    ]))
}
