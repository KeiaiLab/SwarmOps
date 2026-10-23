//! The context every workspace operation runs with.

use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_api::{Identity, StoreOps};
use swarmops_store::FsOps;

use crate::hostfs::HostFs;

/// What every workspace operation runs with: the store, where it is, who acts, and the two file
/// layers. The CLI builds it from `--repo` / `SWARMOPS_REPO` or the workspace's `repo` and the
/// caller's identity, if any; the MCP server from its own store and identity.
#[non_exhaustive]
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

impl<'a> Ctx<'a> {
    /// A context on `store`, whose root on the host is `store_root`, acting as `who`, over the
    /// durable files' `fs` and the materialised tree's `host`.
    pub fn new(
        store: &'a dyn StoreOps,
        store_root: Utf8PathBuf,
        who: Option<Identity>,
        fs: Arc<dyn FsOps>,
        host: Arc<dyn HostFs>,
    ) -> Ctx<'a> {
        Ctx {
            store,
            store_root,
            who,
            fs,
            host,
        }
    }
}
