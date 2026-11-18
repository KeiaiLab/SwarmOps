//! The context every workspace operation runs with.

use std::sync::Arc;

use camino::Utf8PathBuf;
use swarmops_api::{
    Identity, OpError, QueuePublishReq, QueuePublishResp, QueueRoundReq, QueueRoundResp,
    QueueVerdictReq, StoreOps,
};
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
    /// The store's side of a landing queue's runner (the M22 core design §5); none until
    /// [`Ctx::with_queue`] gives one, and `queue run` then answers `E_INVALID_ARG`.
    pub queue: Option<&'a dyn QueueDriver>,
}

/// A target's lane, held until dropped: its holder is the target's one queue runner (the M22
/// core design I-4).
pub trait LaneGuard {
    /// Records `groups`, the slot and process group id of each check running now, so
    /// that the next holder kills what a dead runner left running (design §5 step 5, the
    /// takeover). Best effort: a group not recorded is one a takeover cannot kill, and the
    /// queue's facts are whole either way. The default records nothing.
    fn running(&self, groups: &[(u16, u32)]) {
        let _ = groups;
    }
}

/// What a landing queue's runner asks of the store (the M22 core design §5): its lane, and the
/// three short frames of a round. The checks run between these calls, with no store lock held.
///
/// ```text
///  try_lane ─▶ plan_round ─▶ plan_round(probes) ─▶ checks ─▶ record_verdict … ─▶ publish
///     ▲                                                                            │
///     └──────────────── lane dropped, queue_status refolded, work left ◀───────────┘
/// ```
pub trait QueueDriver {
    /// Takes `target`'s lane without waiting: `None` while another runner holds it. A new
    /// holder kills what a dead one left running first.
    fn try_lane(&self, target: &str) -> Result<Option<Box<dyn LaneGuard>>, OpError>;

    /// Whether no runner holds `target`'s lane.
    fn lane_free(&self, target: &str) -> Result<bool, OpError>;

    /// Builds the target's open entries into a chain on its head, and records the candidates
    /// `req.probes` names.
    fn plan_round(&self, req: QueueRoundReq) -> Result<QueueRoundResp, OpError>;

    /// Records what checking a candidate tree came to.
    fn record_verdict(&self, req: QueueVerdictReq) -> Result<(), OpError>;

    /// Moves the target to a checked prefix and records its entries landed and the removals,
    /// in one frame or not at all.
    fn publish(&self, req: QueuePublishReq) -> Result<QueuePublishResp, OpError>;
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
            queue: None,
        }
    }

    /// This context with `queue` as its landing queue's store side.
    #[must_use]
    pub fn with_queue(mut self, queue: &'a dyn QueueDriver) -> Ctx<'a> {
        self.queue = Some(queue);
        self
    }
}
