//! `push` (M13; operations spec §1's `push` row, §6): a store ref sent to a remote branch by
//! compare-and-swap, never by force. The ref is exported into a staging branch of the mirror
//! seeded at what the last fetch saw, so the export only appends, then git pushes it with a
//! lease on that value: the remote moves only from it, to a descendant of it.
//!
//! A client-side operation, as `git export` is, and an MCP tool, which takes a remote by its
//! name only. Its public form takes the mirror's lock; the locked form receives it from `sync`,
//! which holds it already.
//!
//! The M13 plan's task C1 froze these signatures; task C7 writes their bodies, which until then
//! fail with `E_INTERNAL` naming it.

use swarmops_api::{OpError, PushReq, PushResp, RemoteInfo};
use swarmops_git::mirror::MirrorLock;

use crate::error::planned;
use crate::local::{LocalOps, Recording, Resolved};

/// The task that writes this module's bodies.
const TASK: &str = "C7";

impl LocalOps {
    /// `push` as a request of its own ([`Recording::push`]).
    pub fn push(&self, req: PushReq) -> Result<PushResp, OpError> {
        self.recording(&Resolved::default()).push(req)
    }
}

impl Recording<'_> {
    /// `push`: `req.source_ref` sent to the remote's branch by compare-and-swap, under the
    /// mirror's lock, which it takes. It needs no identity: it is an export, as git export is.
    pub fn push(&self, _req: PushReq) -> Result<PushResp, OpError> {
        Err(planned(TASK))
    }

    /// `push` to `remote` under the mirror's `lock`, which the caller holds.
    #[allow(
        dead_code,
        clippy::unused_self,
        reason = "push (M13 C7) and sync (C8b) call it"
    )]
    pub(crate) fn push_locked(
        &self,
        _lock: &MirrorLock,
        _remote: &RemoteInfo,
        _req: PushReq,
    ) -> Result<PushResp, OpError> {
        Err(planned(TASK))
    }
}
