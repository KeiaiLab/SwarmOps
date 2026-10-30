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
//! ```text
//!   git/lock, once                       E_BUSY at once when held
//!   attempt 1..max_attempts:
//!     M = main                           read first; the compare-and-swap below guards it
//!     fetch (its locked form)            → C_R; skipped after a lost move of main
//!     no main          → main := C_R                           fast_forwarded
//!     M = C_R          → nothing                               up_to_date
//!     nothing above B  → main: M → C_R                         fast_forwarded
//!     B = C_R          → H = M (the remote did not move)
//!     otherwise        → drop what the remote holds, land the rest onto C_R → H
//!                        main: M → H       (lost: again, without a fetch)
//!     mode local       → stop                                  landed_locally
//!     push main (its locked form)          (lease lost: again, with a fetch)
//!                                                              pushed
//!   used up → E_CAS_RETRY_EXHAUSTED or E_REMOTE_DIVERGED remote_moved, with main_moved
//! ```

use serde_json::Value;
use swarmops_api::{
    DropReason, DroppedChange, ErrorCode, HeadMove, Identity, LogReq, MAX_LAND_ATTEMPTS, OpError,
    PublishedChange, PushReq, RemoteHead, RemoteInfo, RemoteListReq, Rev, StoreOps as _, SyncMode,
    SyncReq, SyncResp, SyncStatus, SyncStatusKind, SyncStatusReq,
};
use swarmops_core::{ChangesetId, Signature};
use swarmops_git::mirror::{Mirror, MirrorLock};
use swarmops_git::sync::{self as engine, MAX_SYNC_WALK, SyncError};
use swarmops_git::{ExportOid, GitOid};
use swarmops_repo::{CasResult, RepoError};
use swarmops_store::SYSTEM_GIT;

use crate::error;
use crate::local::{LocalOps, Recording, Resolved};

/// The store ref a sync lands onto the remote's head and publishes.
const MAIN: &str = "main";

/// The reason of a push whose lease failed: the remote moved since the fetch.
const REMOTE_MOVED: &str = "remote_moved";

/// The detail that says whether `main` names what the sync landed.
const MAIN_MOVED: &str = "main_moved";

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
    pub fn sync(&self, who: &Identity, req: SyncReq) -> Result<SyncResp, OpError> {
        let SyncReq {
            remote,
            mode,
            max_attempts,
            ..
        } = req;

        // 1. The request's rules, the remote and the lander's signature, before the lock.
        attempts_in_range(max_attempts)?;
        let remote = self.sync_remote(&remote)?;
        let lander = self.ops.native_signature(who, None)?;

        // 2. The mirror's lock, once, held by every attempt.
        let mirror = Mirror::open(self.ops.store_path()).map_err(error::mirror_error)?;
        let lock = mirror.lock().map_err(error::mirror_error)?;
        let mut syncing = Syncing {
            rec: self,
            lock: &lock,
            mirror: &mirror,
            remote: &remote,
            who,
            lander,
            mode,
            main_old: None,
            remote_old: None,
            landed: None,
            warnings: Vec::new(),
        };
        syncing.run(max_attempts)
    }

    /// `sync status`: how `main` and the remote's followed branch stand, from the store and the
    /// mirror alone: no network, no lock.
    pub fn sync_status(&self, req: SyncStatusReq) -> Result<SyncStatus, OpError> {
        let SyncStatusReq { remote, limit, .. } = req;
        let remote = self.sync_remote(&remote)?;

        // 1. What the last fetch saw, and what main names.
        let seen = self.seen(&remote)?;
        let head = match seen {
            Some(oid) => self.known(oid)?.map(|changeset| (oid, changeset)),
            None => None,
        };
        let main = self.main_now()?;

        // 2. The base, and how far each side is above it.
        let standing = match (main, head) {
            (Some(main), Some((_, theirs))) => self.standing(&remote, main, theirs)?,
            (Some(main), None) => {
                let (ahead, cut) = self.chain_len(main)?;
                Standing {
                    base: None,
                    ahead,
                    behind: 0,
                    truncated: cut,
                }
            }
            (None, Some((_, theirs))) => {
                let (behind, cut) = self.chain_len(theirs)?;
                Standing {
                    base: None,
                    ahead: 0,
                    behind,
                    truncated: cut,
                }
            }
            (None, None) => Standing::default(),
        };

        // 3. The unpublished changes, newest first, as `log` gives them.
        let listed = usize::try_from(standing.ahead)
            .unwrap_or(usize::MAX)
            .min(limit);
        let unpublished = match main {
            Some(main) if listed > 0 => {
                let mut req = LogReq::default();
                req.rev = Some(Rev::from(main.to_string()));
                req.limit = listed;
                let page = self.log(req)?;
                page.entries
            }
            _ => Vec::new(),
        };

        // 4. The response.
        let truncated =
            standing.truncated || u64::try_from(limit).is_ok_and(|n| standing.ahead > n);
        let mut out = SyncStatus::new(
            remote.name,
            remote.branch,
            seen.is_some(),
            standing.ahead,
            standing.behind,
            unpublished,
            truncated,
        );
        out.remote_head = head.map(|(oid, changeset)| remote_head(oid, changeset));
        out.base = standing.base.map(|base| base.to_string());
        Ok(out)
    }

    /// The remote named `name`, as `remote list` reads it, or `E_REMOTE_NOT_FOUND`.
    fn sync_remote(&self, name: &str) -> Result<RemoteInfo, OpError> {
        let listed = self.remote_list(RemoteListReq::default())?;
        let found = listed
            .remotes
            .into_iter()
            .find(|remote| remote.name == name);
        found.ok_or_else(|| error::remote_not_found(name))
    }

    /// What the mirror's tracking ref of `remote`'s followed branch names: none before the
    /// first fetch made the mirror, or saw the branch. Read without the lock.
    fn seen(&self, remote: &RemoteInfo) -> Result<Option<GitOid>, OpError> {
        let mirror = Mirror::open(self.ops.store_path()).map_err(error::mirror_error)?;
        if !mirror.git_dir().exists() {
            return Ok(None);
        }
        mirror
            .tracking(&remote.name, &remote.branch)
            .map_err(error::mirror_error)
    }

    /// The changeset the store imported the commit `oid` as, if it did.
    fn known(&self, oid: GitOid) -> Result<Option<ChangesetId>, OpError> {
        let entry = self
            .ops
            .repo
            .external_id(SYSTEM_GIT, &oid.0)
            .map_err(error::op)?;
        Ok(entry.map(|entry| entry.changeset))
    }

    /// What `main` names now, if it exists.
    fn main_now(&self) -> Result<Option<ChangesetId>, OpError> {
        let state = self.ops.repo.get_ref(MAIN).map_err(error::op)?;
        Ok(state.map(|state| state.target))
    }

    /// Where `main` (at `main`) and the remote's head (`theirs`) stand: the engine's plan, or,
    /// when they share nothing within its walk, each side's whole first-parent chain.
    fn standing(
        &self,
        remote: &RemoteInfo,
        main: ChangesetId,
        theirs: ChangesetId,
    ) -> Result<Standing, OpError> {
        match engine::plan(&self.ops.repo, main, theirs) {
            Ok(plan) => Ok(Standing {
                base: Some(plan.base),
                ahead: to_u64(plan.unpublished.len()),
                behind: plan.behind,
                truncated: plan.behind_truncated,
            }),
            Err(SyncError::NoCommonBase) => {
                let (ahead, ahead_cut) = self.chain_len(main)?;
                let (behind, behind_cut) = self.chain_len(theirs)?;
                Ok(Standing {
                    base: None,
                    ahead,
                    behind,
                    truncated: ahead_cut || behind_cut,
                })
            }
            Err(e) => Err(error::sync_error(e, &remote.name, &remote.branch)),
        }
    }

    /// The length of `start`'s first-parent chain, `start` included, counted up to
    /// [`MAX_SYNC_WALK`]; and whether it stopped there.
    fn chain_len(&self, start: ChangesetId) -> Result<(u64, bool), OpError> {
        let mut count = 0;
        let mut at = Some(start);
        while let Some(id) = at {
            if count >= MAX_SYNC_WALK {
                return Ok((to_u64(count), true));
            }
            count += 1;
            let meta = self.ops.repo.changeset_meta(id).map_err(error::op)?;
            let meta = meta.ok_or_else(|| error::op(RepoError::NotFound(id.to_string())))?;
            at = meta.parents.first().copied();
        }
        Ok((to_u64(count), false))
    }
}

/// Where `main` and the remote's head stand, as `sync status` counts it.
#[derive(Debug, Default)]
struct Standing {
    /// The newest changeset on `main`'s first-parent chain that the remote's head reaches.
    base: Option<ChangesetId>,
    /// `main`'s first parents above the base.
    ahead: u64,
    /// The remote head's first parents above the base.
    behind: u64,
    /// Whether a count stopped at [`MAX_SYNC_WALK`].
    truncated: bool,
}

/// One sync under the mirror's lock: what its attempts share.
struct Syncing<'s, 'r> {
    /// The request's operations.
    rec: &'s Recording<'r>,
    /// The mirror's lock, held for the whole sync.
    lock: &'s MirrorLock,
    /// The store's mirror.
    mirror: &'s Mirror,
    /// The remote.
    remote: &'s RemoteInfo,
    /// The caller, who moves `main`.
    who: &'s Identity,
    /// The committer of each landed change.
    lander: Signature,
    /// Whether the sync pushes.
    mode: SyncMode,
    /// `main` as the sync found it.
    main_old: Option<ChangesetId>,
    /// The remote's head as the sync's first fetch found it.
    remote_old: Option<GitOid>,
    /// What this sync last moved `main` to.
    landed: Option<ChangesetId>,
    /// What the sync has to say that did not stop it.
    warnings: Vec<String>,
}

/// What one attempt came to.
enum Step {
    /// The sync is done.
    Done(SyncResp),
    /// `main` moved under it: again, with the same fetch.
    MainMoved,
    /// The remote moved since the fetch: again, with a new fetch.
    RemoteMoved(OpError),
}

/// What a sync landed onto the remote's head: the head, each change with what it landed as, and
/// what the remote held.
struct Landed {
    /// What `main` moves to.
    head: ChangesetId,
    /// Each change, and the changeset it landed as (itself when the remote did not move).
    published: Vec<(ChangesetId, ChangesetId)>,
    /// The changes the remote held.
    dropped: Vec<DroppedChange>,
}

impl Syncing<'_, '_> {
    /// The attempts, up to `max_attempts`.
    fn run(&mut self, max_attempts: u32) -> Result<SyncResp, OpError> {
        let mut fetched = None;
        let mut lost = None;
        for attempt in 1..=max_attempts {
            // 1. main first: a move under the attempt fails its compare-and-swap.
            let main = self.rec.main_now()?;
            if attempt == 1 {
                self.main_old = main;
            }

            // 2. The remote's head, fetched again unless main's move was what was lost.
            let head = match fetched {
                Some(head) => head,
                None => self.fetch().map_err(|e| self.said(e))?,
            };
            fetched = Some(head);
            if attempt == 1 {
                self.remote_old = head.map(|(oid, _)| oid);
            }

            // 3. The attempt.
            let step = self
                .attempt(attempt, main, head)
                .map_err(|e| self.said(e))?;
            match step {
                Step::Done(resp) => return Ok(resp),
                Step::MainMoved => lost = None,
                Step::RemoteMoved(e) => {
                    fetched = None;
                    lost = Some(e);
                }
            }
        }
        Err(self.exhausted(max_attempts, lost)?)
    }

    /// The fetch's locked form; the followed branch's head and its changeset, if the remote has
    /// the branch.
    fn fetch(&mut self) -> Result<Option<(GitOid, ChangesetId)>, OpError> {
        let fetched = self.rec.fetch_locked(self.lock, self.remote)?;
        self.warnings.extend(fetched.warnings.unwrap_or_default());

        // What the fetch left in the tracking ref, as the store knows it.
        let seen = self
            .mirror
            .tracking(&self.remote.name, &self.remote.branch)
            .map_err(error::mirror_error)?;
        let Some(oid) = seen else {
            return Ok(None);
        };
        let changeset = self.rec.known(oid)?.ok_or_else(|| {
            error::error(
                ErrorCode::Internal,
                format!("the fetch left {oid}, which the store did not import"),
            )
        })?;
        Ok(Some((oid, changeset)))
    }

    /// One attempt from `main` and the remote's `head`.
    fn attempt(
        &mut self,
        attempt: u32,
        main: Option<ChangesetId>,
        head: Option<(GitOid, ChangesetId)>,
    ) -> Result<Step, OpError> {
        // 1. A remote without the branch: main is pushed as it is, which creates it.
        let Some((sha, theirs)) = head else {
            return self.onto_nothing(attempt, main);
        };

        // 2. No main: main is the remote's head.
        let Some(main) = main else {
            if !self.move_main(None, theirs)? {
                return Ok(Step::MainMoved);
            }
            let done = self.response(SyncStatusKind::FastForwarded, attempt, sha, theirs);
            return Ok(Step::Done(done));
        };

        // 3. Nothing unpublished: up to date, or a fast-forward to the remote's head.
        let plan = engine::plan(&self.rec.ops.repo, main, theirs).map_err(|e| self.engine(e))?;
        if plan.unpublished.is_empty() {
            if main == theirs {
                let done = self.response(SyncStatusKind::UpToDate, attempt, sha, main);
                return Ok(Step::Done(done));
            }
            if !self.move_main(Some(main), theirs)? {
                return Ok(Step::MainMoved);
            }
            let done = self.response(SyncStatusKind::FastForwarded, attempt, sha, theirs);
            return Ok(Step::Done(done));
        }

        // 4. What lands onto the remote's head, and main moved there.
        let landed = if plan.base == theirs {
            Landed {
                head: main,
                published: plan.unpublished.iter().map(|&id| (id, id)).collect(),
                dropped: Vec::new(),
            }
        } else {
            self.land(&plan, theirs)?
        };
        if landed.head != main && !self.move_main(Some(main), landed.head)? {
            return Ok(Step::MainMoved);
        }

        // 5. Everything held already, or no push asked for: done without git.
        let published = self.published(&landed.published)?;
        if landed.head == theirs || self.mode == SyncMode::Local {
            let kind = if landed.head == theirs {
                SyncStatusKind::FastForwarded
            } else {
                SyncStatusKind::LandedLocally
            };
            let mut done = self.response(kind, attempt, sha, landed.head);
            done.published = published;
            done.dropped = landed.dropped;
            return Ok(Step::Done(done));
        }

        // 6. The push, against what this attempt fetched.
        let new = match self.push() {
            Ok(new) => new,
            Err(e) if remote_moved(&e) => return Ok(Step::RemoteMoved(e)),
            Err(e) => return Err(e),
        };
        let mut done = self.response(SyncStatusKind::Pushed, attempt, new, landed.head);
        done.published = published;
        done.dropped = landed.dropped;
        Ok(Step::Done(done))
    }

    /// An attempt onto a remote that has no followed branch: `main` is pushed, which creates the
    /// branch; there is nothing to land onto, so no push is nothing to do.
    fn onto_nothing(&mut self, attempt: u32, main: Option<ChangesetId>) -> Result<Step, OpError> {
        let branch = &self.remote.branch;
        let remote = error::shell_word(&self.remote.name);
        let Some(main) = main else {
            return Err(error::error(
                ErrorCode::RevNotFound,
                format!("neither main nor {branch} on the remote exists: nothing to sync"),
            )
            .with_details(error::details([("rev", MAIN.into())])));
        };
        if self.mode == SyncMode::Local {
            return Err(error::error(
                ErrorCode::RevNotFound,
                format!("the remote has no branch {branch} to land main onto"),
            )
            .with_details(error::details([("rev", branch.as_str().into())]))
            .with_hint(error::hint(format!(
                "swarmops push {remote} main creates it"
            ))));
        }

        // The push creates the branch.
        let new = match self.push() {
            Ok(new) => new,
            Err(e) if remote_moved(&e) => return Ok(Step::RemoteMoved(e)),
            Err(e) => return Err(e),
        };
        Ok(Step::Done(self.response(
            SyncStatusKind::Pushed,
            attempt,
            new,
            main,
        )))
    }

    /// Drops the changes of `plan` the remote at `theirs` holds, and lands the rest onto it.
    fn land(&self, plan: &engine::SyncPlan, theirs: ChangesetId) -> Result<Landed, OpError> {
        let repo = &self.rec.ops.repo;

        // 1. What the remote holds.
        let recognised = engine::recognise(repo, plan, theirs).map_err(|e| self.engine(e))?;
        let mut dropped: Vec<DroppedChange> = (recognised.dropped.iter())
            .map(|&(id, by)| Ok(held(id, drop_reason(by)?)))
            .collect::<Result<_, OpError>>()?;

        // 2. The rest, landed in order; a change that merges as none is dropped too.
        let relocated = engine::relocate(repo, &recognised.to_land, theirs, &self.lander)
            .map_err(|e| self.engine(e))?;
        dropped.extend((relocated.no_ops.iter()).map(|&id| held(id, DropReason::NoOp)));
        Ok(Landed {
            head: relocated.head,
            published: relocated.landed,
            dropped,
        })
    }

    /// Moves `main` from `old` (none: makes it) to `new` by compare-and-swap, as the caller,
    /// recorded in the journal and the op log as `ref set` records it; false when `main` named
    /// something else.
    fn move_main(&mut self, old: Option<ChangesetId>, new: ChangesetId) -> Result<bool, OpError> {
        let outcome = self
            .rec
            .ops
            .repo
            .cas_ref(MAIN, old, Some(new), self.who.agent())
            .map_err(error::op)?;
        self.rec.resolved.record_recovered(outcome.recovered);
        self.warnings.extend(outcome.warnings);

        // Applied, or main names `new` already: a replay.
        let moved = match outcome.result {
            CasResult::Applied { .. } => true,
            CasResult::Mismatch { actual } => actual == Some(new),
            _ => return Err(error::unknown_variant("CasResult")),
        };
        if moved {
            self.landed = Some(new);
            self.rec.resolved.record(MAIN, new);
        }
        Ok(moved)
    }

    /// The push's locked form: `main` to the followed branch; the commit the branch names now.
    fn push(&mut self) -> Result<GitOid, OpError> {
        let mut req = PushReq::new(self.remote.name.clone());
        MAIN.clone_into(&mut req.source_ref);
        let pushed = self.rec.push_locked(self.lock, self.remote, req)?;
        self.warnings.extend(pushed.warnings.unwrap_or_default());
        pushed
            .new
            .parse()
            .map_err(|_| error::error(ErrorCode::Internal, "the push gave no commit id"))
    }

    /// Each change of `landed` with what it landed as and the commit that exports to.
    fn published(
        &self,
        landed: &[(ChangesetId, ChangesetId)],
    ) -> Result<Vec<PublishedChange>, OpError> {
        let repo = &self.rec.ops.repo;
        let mut published = Vec::with_capacity(landed.len());
        for &(change, landed_as) in landed {
            let exported = repo
                .derived::<ExportOid>(landed_as, &())
                .map_err(error::op)?;
            published.push(PublishedChange::new(
                change.to_string(),
                landed_as.to_string(),
                exported.git_oid().to_string(),
            ));
        }
        Ok(published)
    }

    /// The response of a sync that ended as `kind` in `attempt`, the remote at `remote_new` and
    /// `main` at `main_new`; nothing published or dropped yet.
    fn response(
        &mut self,
        kind: SyncStatusKind,
        attempt: u32,
        remote_new: GitOid,
        main_new: ChangesetId,
    ) -> SyncResp {
        let warnings = std::mem::take(&mut self.warnings);
        let mut main = HeadMove::new(main_new.to_string());
        main.old = self.main_old.map(|id| id.to_string());
        let mut remote_head = HeadMove::new(remote_new.to_string());
        remote_head.old = self.remote_old.map(|oid| oid.to_string());
        let mut out = SyncResp::new(
            self.remote.name.clone(),
            self.remote.branch.clone(),
            kind,
            attempt,
            Vec::new(),
            Vec::new(),
            main,
            remote_head,
        );
        out.warnings = (!warnings.is_empty()).then_some(warnings);
        out
    }

    /// The engine's failure `e` as the sync's error.
    fn engine(&self, e: SyncError) -> OpError {
        error::sync_error(e, &self.remote.name, &self.remote.branch)
    }

    /// `e`, an error after this sync moved `main`, saying so; a refusal of the push hints at a
    /// branch of one's own and a pull request.
    fn said(&self, mut e: OpError) -> OpError {
        if self.landed.is_none() {
            return e;
        }
        e.details.insert(MAIN_MOVED.to_owned(), Value::Bool(true));
        if e.code == ErrorCode::RemoteRejected.as_str() {
            let remote = error::shell_word(&self.remote.name);
            e.hint = Some(error::hint(format!(
                "main holds the landed changes: swarmops push {remote} main:<a branch of yours>, \
                 then open a pull request; to take main back, land a change that reverts them"
            )));
        }
        e
    }

    /// The error of a sync whose `attempts` were used up: the remote's last move (`lost`), or
    /// `main`'s; each says whether `main` names what the sync landed.
    fn exhausted(&self, attempts: u32, lost: Option<OpError>) -> Result<OpError, OpError> {
        let moved = self.landed.is_some() && self.rec.main_now()? == self.landed;
        let mut e = match lost {
            Some(e) => e,
            None => error::error(
                ErrorCode::CasRetryExhausted,
                format!("main moved under the sync in each of its {attempts} attempts"),
            )
            .with_details(error::details([
                ("remote", self.remote.name.as_str().into()),
                ("branch", self.remote.branch.as_str().into()),
                ("attempts", attempts.into()),
            ]))
            .with_hint(error::hint(format!(
                "swarmops sync {} again",
                error::shell_word(&self.remote.name)
            ))),
        };
        e.details.insert(MAIN_MOVED.to_owned(), Value::Bool(moved));
        Ok(e)
    }
}

/// `E_INVALID_ARG` unless `max_attempts` is 1 to [`MAX_LAND_ATTEMPTS`], as `land`'s.
fn attempts_in_range(max_attempts: u32) -> Result<(), OpError> {
    if (1..=MAX_LAND_ATTEMPTS).contains(&max_attempts) {
        return Ok(());
    }
    Err(error::error(
        ErrorCode::InvalidArg,
        format!("max_attempts is {max_attempts}, not 1 to {MAX_LAND_ATTEMPTS}"),
    )
    .with_details(error::details([("max_attempts", max_attempts.into())])))
}

/// Whether `e` is a push whose lease failed: the remote moved since the fetch.
fn remote_moved(e: &OpError) -> bool {
    e.code == ErrorCode::RemoteDiverged.as_str()
        && e.details.get("reason").and_then(Value::as_str) == Some(REMOTE_MOVED)
}

/// A change the remote held, found `by`.
fn held(id: ChangesetId, by: DropReason) -> DroppedChange {
    DroppedChange::new(id.to_string(), by)
}

/// The engine's reason as the wire gives it. `E_INTERNAL` for a reason this build does not
/// know.
fn drop_reason(by: engine::DropReason) -> Result<DropReason, OpError> {
    match by {
        engine::DropReason::Trailer => Ok(DropReason::Trailer),
        engine::DropReason::NoOp => Ok(DropReason::NoOp),
        _ => Err(error::unknown_variant("DropReason")),
    }
}

/// The remote's head `oid`, the changeset `changeset`, as the wire gives it.
fn remote_head(oid: GitOid, changeset: ChangesetId) -> RemoteHead {
    RemoteHead::new(oid.to_string(), changeset.to_string())
}

/// `n` as a count on the wire.
fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
