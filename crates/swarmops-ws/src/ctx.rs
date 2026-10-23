//! The context every workspace operation runs with.

use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_api::{Identity, StoreOps};
use swarmops_store::FsOps;

use crate::hostfs::HostFs;

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
