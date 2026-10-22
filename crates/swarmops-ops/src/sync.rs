//! `sync` and `sync status` (M13; operations spec §1's `sync` and `sync_status` rows): a
//! remote fetched, `main`'s unpublished changes landed onto its head on the client, `main`
//! moved there and pushed by compare-and-swap, again when the remote moved meanwhile; and,
//! offline, how `main` and the remote stand as the last fetch saw it.
//!
//! The engine (the base, the changes the remote holds, the relocation) is
//! [`swarmops_git::sync`]; this module takes the mirror's lock once, runs the fetch's and the
//! push's locked forms under it, and moves `main` with this request's own ref move, so the
//! journal, the op log and the envelope see it. Both are MCP tools, which take a remote by its
//! name only.
//!
//! The M13 plan's task C1 froze these signatures; task C8b writes their bodies, which until then
//! fail with `E_INTERNAL` naming it.

use swarmops_api::{Identity, OpError, SyncReq, SyncResp, SyncStatus, SyncStatusReq};

use crate::error::planned;
use crate::local::{LocalOps, Recording, Resolved};

/// The task that writes this module's bodies.
const TASK: &str = "C8b";

impl LocalOps {
    /// `sync` as a request of its own ([`Recording::sync`]).
    pub fn sync(&self, who: &Identity, req: SyncReq) -> Result<SyncResp, OpError> {
        self.recording(&Resolved::default()).sync(who, req)
    }

    /// `sync status` as a request of its own ([`Recording::sync_status`]).
    pub fn sync_status(&self, req: SyncStatusReq) -> Result<SyncStatus, OpError> {
        self.recording(&Resolved::default()).sync_status(req)
    }
}

impl Recording<'_> {
    /// `sync`: fetch `req.remote`, land `main`'s unpublished changes onto its head, move `main`
    /// there by `who`, and push it (unless `req.mode` is `local`), under the mirror's lock,
    /// which it takes once. It needs an identity: it lands and moves `main`.
    pub fn sync(&self, _who: &Identity, _req: SyncReq) -> Result<SyncResp, OpError> {
        Err(planned(TASK))
    }

    /// `sync status`: how `main` and the remote's followed branch stand, from the store and the
    /// mirror alone: no network, no lock.
    pub fn sync_status(&self, _req: SyncStatusReq) -> Result<SyncStatus, OpError> {
        Err(planned(TASK))
    }
}
