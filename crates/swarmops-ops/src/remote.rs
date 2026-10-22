//! The git remotes of a store (M13): `remotes.toml`, `remote add`, `remote remove`,
//! `remote list`, `fetch` and `clone` (operations spec §1's `clone_repo`, `remote_*` and
//! `fetch` rows, §6).
//!
//! Client-side operations, as `git import` is: each spawns the user's git on this host against
//! the store's mirror, so none is a [`StoreOps`](swarmops_api::StoreOps) operation; the MCP
//! server offers `remote_list` and `fetch`, which take a remote by its name only. The commands
//! that change the remotes or the mirror hold the mirror's lock for their whole run: the public
//! forms take it, and the locked forms (`*_locked`) receive it from a command that holds it
//! already (`clone`, and `sync`'s fetch), since it is not re-entrant.
//!
//! The M13 plan's task C1 froze these signatures; task C6 writes their bodies, which until then
//! fail with `E_INTERNAL` naming it.

use swarmops_api::{
    CloneReq, CloneResp, FetchReq, FetchResp, Identity, OpError, RemoteAddReq, RemoteInfo,
    RemoteList, RemoteListReq, RemoteRemoveReq, RemoteRemoveResp,
};
use swarmops_git::mirror::MirrorLock;

use crate::error::planned;
use crate::local::{LocalOps, Recording, Resolved};

/// The task that writes this module's bodies.
const TASK: &str = "C6";

impl LocalOps {
    /// `clone`: a store made at `req.path` (or, to resume an interrupted clone, the one there),
    /// `req.url` recorded as `req.remote`, fetched, and its followed branch imported into
    /// `main` by `who`; the ref move is recorded in `resolved`. An associated function, as
    /// [`LocalOps::init`] is: there is no store to open before it. It needs an identity: it
    /// moves `main`.
    pub fn clone_repo(
        _req: CloneReq,
        _who: &Identity,
        _resolved: &Resolved,
    ) -> Result<(LocalOps, CloneResp), OpError> {
        Err(planned(TASK))
    }

    /// `remote add` as a request of its own ([`Recording::remote_add`]).
    pub fn remote_add(&self, req: RemoteAddReq) -> Result<RemoteInfo, OpError> {
        self.recording(&Resolved::default()).remote_add(req)
    }

    /// `remote remove` as a request of its own ([`Recording::remote_remove`]).
    pub fn remote_remove(&self, req: RemoteRemoveReq) -> Result<RemoteRemoveResp, OpError> {
        self.recording(&Resolved::default()).remote_remove(req)
    }

    /// `remote list` as a request of its own ([`Recording::remote_list`]).
    pub fn remote_list(&self, req: RemoteListReq) -> Result<RemoteList, OpError> {
        self.recording(&Resolved::default()).remote_list(req)
    }

    /// `fetch` as a request of its own ([`Recording::fetch`]).
    pub fn fetch(&self, req: FetchReq) -> Result<FetchResp, OpError> {
        self.recording(&Resolved::default()).fetch(req)
    }
}

impl Recording<'_> {
    /// `remote add`: records `req.name` at `req.url` in `remotes.toml`, under the mirror's lock,
    /// which it takes. It needs no identity: it writes remotes.toml and the mirror, no store
    /// object.
    pub fn remote_add(&self, _req: RemoteAddReq) -> Result<RemoteInfo, OpError> {
        Err(planned(TASK))
    }

    /// `remote remove`: forgets `req.name`, and the mirror's refs of it, under the mirror's
    /// lock, which it takes. It needs no identity: it writes remotes.toml and the mirror, no
    /// store object.
    pub fn remote_remove(&self, _req: RemoteRemoveReq) -> Result<RemoteRemoveResp, OpError> {
        Err(planned(TASK))
    }

    /// `remote list`: the remotes `remotes.toml` records, read without the lock.
    pub fn remote_list(&self, _req: RemoteListReq) -> Result<RemoteList, OpError> {
        Err(planned(TASK))
    }

    /// `fetch`: the remote's branches and tags into the mirror, and its followed branch's
    /// commits into the store, under the mirror's lock, which it takes. It needs no identity:
    /// it moves no ref and records no agent.
    pub fn fetch(&self, _req: FetchReq) -> Result<FetchResp, OpError> {
        Err(planned(TASK))
    }

    /// `remote add` under the mirror's `lock`, which the caller holds.
    #[allow(dead_code, clippy::unused_self, reason = "clone (M13 C6) calls it")]
    pub(crate) fn remote_add_locked(
        &self,
        _lock: &MirrorLock,
        _req: RemoteAddReq,
    ) -> Result<RemoteInfo, OpError> {
        Err(planned(TASK))
    }

    /// `fetch` of `remote` under the mirror's `lock`, which the caller holds.
    #[allow(
        dead_code,
        clippy::unused_self,
        reason = "fetch, clone (M13 C6) and sync (C8b) call it"
    )]
    pub(crate) fn fetch_locked(
        &self,
        _lock: &MirrorLock,
        _remote: &RemoteInfo,
    ) -> Result<FetchResp, OpError> {
        Err(planned(TASK))
    }
}
