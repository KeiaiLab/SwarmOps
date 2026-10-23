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
mod ctx;
pub mod delete;
pub mod diff;
pub mod discard;
pub mod hook;
pub mod hostfs;
pub mod index;
pub mod info;
mod io_error;
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

pub use commit::commit;
pub use create::create;
pub use ctx::Ctx;
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
