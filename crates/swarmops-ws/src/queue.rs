//! The landing queue's workspace side (the M22 core design §5, §6): `enqueue`, refused without
//! a check to run, and the runner of one target.
//!
//! ```text
//!  run: try_lane ──none──▶ return (another runner drains it)
//!         │
//!         ▼ round
//!       plan_round ─▶ decide ─▶ green prefix or a blamed entry? ─▶ publish ─▶ round
//!                         │
//!                         ▼ probes
//!       plan_round(probes) ─▶ checks, at most `inflight` at once, each in its workspace
//!                              (no store lock held) ─▶ record_verdict as each ends ─▶ round
//!       no open entry ─▶ lane dropped ─▶ queue_status: work left? ─▶ try_lane again
//! ```
//!
//! An entry is blamed only when its prefix is red and the one before is green (I-9); a check
//! killed from outside is `cancelled` and counts on its entry only when it is that next entry,
//! its third such verdict removing it.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use swarmops_api::{
    DEFAULT_CHECK_TIMEOUT_S, ErrorCode, OpError, QueueEnqueueReq, QueueEnqueueResp,
    QueueEntryState, QueuePublishReq, QueuePublishStatus, QueueRemovalIn, QueueRoundItem,
    QueueRoundReq, QueueStatusReq, QueueVerdictReq, RemovalReason, StatusAfter, VerdictState,
    WsCreateReq, WsDeleteReq,
};

use crate::probe::{self, End, Known};
use crate::{Ctx, QueueDriver, check, create, delete};

/// How many candidates a runner checks at once unless told otherwise (the prototype's batch).
pub const QUEUE_INFLIGHT_DEFAULT: u32 = 4;

/// The most candidates a runner checks at once: a chain's most changes.
pub const QUEUE_INFLIGHT_MAX: u32 = 32;

/// The cancelled verdicts after which an entry is removed (reason `cancelled`).
pub const CANCEL_LIMIT: u32 = 3;

/// How many hex digits of a candidate's tree name the workspace it is checked in.
const WS_TREE_DIGITS: usize = 16;

/// How a runner runs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunReq {
    /// The target ref whose queue it drains.
    pub target: String,
    /// How many candidates it checks at once, 1 to [`QUEUE_INFLIGHT_MAX`].
    pub inflight: u32,
    /// How long one check may run.
    pub check_timeout: Duration,
    /// The directory its checks' workspaces are made in.
    pub dir: Utf8PathBuf,
}

impl RunReq {
    /// A runner of `target`'s queue, its workspaces in `dir`; the others the defaults.
    pub fn new(target: impl Into<String>, dir: impl Into<Utf8PathBuf>) -> RunReq {
        RunReq {
            target: target.into(),
            inflight: QUEUE_INFLIGHT_DEFAULT,
            check_timeout: Duration::from_secs(DEFAULT_CHECK_TIMEOUT_S),
            dir: dir.into(),
        }
    }
}

/// What a runner did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunResp {
    /// The rounds that checked candidates.
    pub rounds: u32,
    /// The checks it ran.
    pub checks: u32,
    /// The publishes the store recorded.
    pub publishes: u32,
}

/// `queue_enqueue`: refused with `E_INVALID_ARG` (`details.missing` `check`) when the change's
/// head names no check, before the store is asked; else the store's answer.
pub fn enqueue(ctx: &Ctx<'_>, req: QueueEnqueueReq) -> Result<QueueEnqueueResp, OpError> {
    // 1. Who asks: the entry is a store write.
    let Some(who) = ctx.who.as_ref() else {
        return Err(OpError::new(
            ErrorCode::IdentityRequired,
            "queue enqueue needs an identity: it records who asked",
        )
        .with_hint("pass --as <family>/<instance>, or set SWARMOPS_AGENT"));
    };

    // 2. A check to run, read from the store at the change's head.
    let head = req.head.as_str().to_owned();
    check::configured(ctx.store, &head)?.ok_or_else(check::missing)?;

    // 3. The store's entry.
    ctx.store.queue_enqueue(who, req)
}

/// `queue run`: drains `req.target`'s queue while it holds its lane; returns at once when
/// another runner holds it. `E_INVALID_ARG` when the context has no queue driver, or for an
/// `inflight` outside 1 to [`QUEUE_INFLIGHT_MAX`].
pub fn run(ctx: &Ctx<'_>, req: &RunReq) -> Result<RunResp, OpError> {
    // 1. A driver, a width, the workspaces' directory.
    let Some(driver) = ctx.queue else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "this store does not have the queue run operation",
        ));
    };
    if !(1..=QUEUE_INFLIGHT_MAX).contains(&req.inflight) {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            format!("--inflight must be 1 to {QUEUE_INFLIGHT_MAX}"),
        ));
    }
    std::fs::create_dir_all(&req.dir).map_err(|e| host_failed(&req.dir, &e))?;

    // 2. Drain while the lane is held; after it is dropped, look again (I-6): an entry
    //    enqueued while it was held found it taken and started no runner.
    let mut out = RunResp::default();
    loop {
        let Some(lane) = driver.try_lane(&req.target)? else {
            return Ok(out);
        };
        let drained = Runner {
            ctx,
            driver,
            req,
            out: &mut out,
        }
        .drain();
        drop(lane);
        drained?;
        if !has_open(ctx, &req.target)? {
            return Ok(out);
        }
    }
}

/// Whether `target`'s queue holds an open entry, as the store's facts say now.
fn has_open(ctx: &Ctx<'_>, target: &str) -> Result<bool, OpError> {
    let mut req = QueueStatusReq::new();
    req.target = Some(target.to_owned());
    let status = ctx.store.queue_status(req)?;
    let open = (status.entries.iter()).any(|entry| {
        matches!(
            entry.state,
            QueueEntryState::Waiting | QueueEntryState::Checking
        )
    });
    Ok(open)
}

/// One runner, holding its target's lane.
struct Runner<'r, 'a> {
    ctx: &'r Ctx<'a>,
    driver: &'a dyn QueueDriver,
    req: &'r RunReq,
    out: &'r mut RunResp,
}

/// A prefix whose check was cancelled from outside, kept from one round to the next while the
/// head stays: the next probes split the interval before it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Suspect {
    head: String,
    entry: String,
}

/// One check to run: the prefix ending at `place`, from 1, of a round's chain.
struct Job {
    place: u32,
    entry: String,
    tree: String,
    argv: Option<Vec<String>>,
    name: String,
    dir: Utf8PathBuf,
}

impl Runner<'_, '_> {
    /// Rounds until the target's queue has no open entry, or the target is gone.
    fn drain(&mut self) -> Result<(), OpError> {
        let mut suspect: Option<Suspect> = None;
        loop {
            // 1. The chain on the target's head; none left, or the target gone, ends it.
            let round = (self.driver).plan_round(QueueRoundReq::new(&self.req.target))?;
            let Some(head) = round.head else {
                return Ok(());
            };
            if round.items.is_empty() {
                return Ok(());
            }
            let items = round.items;

            // 2. What the verdicts decide: a green prefix lands, a blamed entry goes.
            let known: Vec<Known> = items.iter().map(known).collect();
            let found = probe::decide(&known);
            if self.publish(&head, &items, &found)? {
                suspect = None;
                continue;
            }

            // 3. The probes: inside the interval, or before a prefix cancelled from outside.
            let at = suspect
                .as_ref()
                .filter(|s| s.head == head)
                .and_then(|s| items.iter().position(|item| item.entry == s.entry))
                .map(place);
            let probes = self.probes(&found, at, &known);

            // 4. Recorded as candidates on the same head, then checked.
            let mut ask = QueueRoundReq::new(&self.req.target);
            ask.expect_head = Some(head.clone());
            ask.probes.clone_from(&probes);
            let planned = self.driver.plan_round(ask)?;
            let same = planned.head.as_deref() == Some(head.as_str())
                && (planned.items.iter().map(|item| &item.entry))
                    .take(items.len())
                    .eq(items.iter().map(|item| &item.entry));
            if !same {
                suspect = None;
                continue;
            }
            let cancelled = self.check(&planned.items, &probes)?;
            suspect = cancelled.map(|place| Suspect {
                head: head.clone(),
                entry: planned.items[index(place)].entry.clone(),
            });
        }
    }

    /// Publishes what `found` decides on `head`: the green prefix, and the next entry when its
    /// prefix is red or was cancelled from outside [`CANCEL_LIMIT`] times. Whether it asked.
    fn publish(
        &mut self,
        head: &str,
        items: &[QueueRoundItem],
        found: &probe::Interval,
    ) -> Result<bool, OpError> {
        // 1. The entry after the green prefix, blamed only by a decisive verdict.
        let green = usize::try_from(found.green).unwrap_or(usize::MAX);
        let mut removed = Vec::new();
        if let Some(next) = items.get(green) {
            let reason = if found.red == End::Red && found.end == found.green + 1 {
                Some(blame(next.verdict))
            } else if known(next) == Known::Open && next.cancelled >= CANCEL_LIMIT {
                Some(RemovalReason::Cancelled)
            } else {
                None
            };
            if let Some(reason) = reason {
                let mut removal = QueueRemovalIn::new(next.entry.clone(), reason);
                removal.tree = Some(next.tree.clone());
                removal.check =
                    check::configured(self.ctx.store, &next.candidate)?.map(|argv| argv.join(" "));
                removed.push(removal);
            }
        }
        if green == 0 && removed.is_empty() {
            return Ok(false);
        }

        // 2. One frame: the target to the green prefix's tip, its entries landed, the removal.
        let mut req = QueuePublishReq::new(&self.req.target, head);
        if green > 0 {
            req.tip = Some(items[green - 1].candidate.clone());
            req.landed = (items[..green].iter())
                .map(|item| item.entry.clone())
                .collect();
        }
        req.removed = removed;
        let done = self.driver.publish(req)?;
        if done.status == QueuePublishStatus::Published {
            self.out.publishes += 1;
        }
        Ok(true)
    }

    /// The prefixes to check: inside the undecided interval, or, with a prefix `suspect`
    /// cancelled from outside inside it, before that one (itself when it is next).
    fn probes(&self, found: &probe::Interval, suspect: Option<u32>, known: &[Known]) -> Vec<u32> {
        // 1. The interval, cut at a cancelled prefix.
        let (mut end, mut red) = (found.end, found.red);
        if let Some(at) = suspect.filter(|at| *at > found.green && *at <= found.end) {
            if at == found.green + 1 {
                return vec![at];
            }
            (end, red) = (at, End::Red);
        }

        // 2. Split, decided prefixes passed over; the next one at least.
        let mut probes = probe::select(found.green, end, red, self.req.inflight);
        probes.retain(|place| known.get(index(*place)) == Some(&Known::Open));
        if probes.is_empty() {
            probes.push(found.green + 1);
        }
        probes
    }

    /// Checks the prefixes `probes` names of `items`, each tree once, at most the runner's
    /// width at a time, each in a workspace of its own, recording each verdict as its check
    /// ends. The place of the shortest prefix cancelled from outside before any red one, if any.
    fn check(&mut self, items: &[QueueRoundItem], probes: &[u32]) -> Result<Option<u32>, OpError> {
        // 1. One job per tree, its workspace made on its candidate.
        let mut jobs: Vec<Job> = Vec::new();
        for &place in probes {
            let Some(item) = items.get(index(place)) else {
                continue;
            };
            if jobs.iter().any(|job| job.tree == item.tree) {
                continue;
            }
            jobs.push(self.job(place, item)?);
        }
        self.out.rounds += 1;

        // 2. Run at once, outside any store lock; each verdict recorded as it comes.
        let mut ran: Vec<(u32, VerdictState)> = Vec::new();
        let mut failed = None;
        let (done_tx, done_rx) = mpsc::channel();
        thread::scope(|scope| {
            for job in &jobs {
                let done = done_tx.clone();
                let timeout = self.req.check_timeout;
                scope.spawn(move || {
                    let verdict = match &job.argv {
                        Some(argv) => check::verdict(argv, &job.dir, &job.name, &job.tree, timeout),
                        None => Ok((VerdictState::Failed, check::missing().message)),
                    };
                    let _ = done.send((job, verdict));
                });
            }
            drop(done_tx);
            for (job, verdict) in done_rx {
                self.out.checks += 1;
                let recorded = verdict.and_then(|(state, output)| {
                    ran.push((job.place, state));
                    self.ended(job, state, output)
                });
                if let Err(e) = recorded {
                    failed.get_or_insert(e);
                }
            }
        });

        // 3. The workspaces removed, then the first failure.
        for job in &jobs {
            let mut req = WsDeleteReq::new(job.name.clone());
            req.force = true;
            if let Err(e) = delete(self.ctx, req) {
                failed.get_or_insert(e);
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }

        // 4. A cancel before any red prefix: counted on its entry when it is the next one.
        self.weigh(items, &jobs, &ran)
    }

    /// The shortest prefix of `ran` cancelled from outside, unless a red one is shorter; its
    /// cancel recorded when the green prefix ends just before it, so that only the next entry
    /// is ever counted (I-9).
    fn weigh(
        &self,
        items: &[QueueRoundItem],
        jobs: &[Job],
        ran: &[(u32, VerdictState)],
    ) -> Result<Option<u32>, OpError> {
        // 1. The shortest cancelled prefix, and no red one before it.
        let cancelled = (ran.iter())
            .filter(|(_, state)| *state == VerdictState::Cancelled)
            .map(|(place, _)| *place)
            .min();
        let Some(first) = cancelled else {
            return Ok(None);
        };

        // 2. What the chain's prefixes before it are now, this round's verdicts included.
        let mut before: Vec<Known> = items[..index(first)].iter().map(known).collect();
        for (place, state) in ran {
            let Some(item) = items.get(index(*place)) else {
                continue;
            };
            let now = known_of(Some(*state));
            for (at, other) in items[..index(first)].iter().enumerate() {
                if other.tree == item.tree {
                    before[at] = now;
                }
            }
        }
        let found = probe::decide(&before);
        if found.red == End::Red {
            return Ok(None);
        }

        // 3. Counted only on the entry right after the green prefix.
        if found.green + 1 == first
            && let Some(job) = jobs.iter().find(|job| job.place == first)
        {
            self.verdict(job, VerdictState::Cancelled, String::new())?;
        }
        Ok(Some(first))
    }

    /// What a check that ended does: a decisive verdict recorded at once; a cancel left for
    /// [`Runner::weigh`] once every check ended.
    fn ended(&self, job: &Job, state: VerdictState, output: String) -> Result<(), OpError> {
        if state == VerdictState::Cancelled {
            return Ok(());
        }
        self.verdict(job, state, output)
    }

    /// Records `state` for `job`'s tree on its entry.
    fn verdict(&self, job: &Job, state: VerdictState, output: String) -> Result<(), OpError> {
        let check = match &job.argv {
            Some(argv) => argv.join(" "),
            None => String::new(),
        };
        let mut req = QueueVerdictReq::new(
            &self.req.target,
            job.entry.clone(),
            job.tree.clone(),
            state,
            check,
        );
        req.output = output;
        self.driver.record_verdict(req)
    }

    /// The job of the prefix at `place`, ending with `item`: its check and its workspace, made
    /// anew on the candidate.
    fn job(&self, place: u32, item: &QueueRoundItem) -> Result<Job, OpError> {
        // 1. The check the candidate names.
        let argv = check::configured(self.ctx.store, &item.candidate)?;

        // 2. A workspace named after the tree; one a killed runner left goes first.
        let digits = item.tree.get(..WS_TREE_DIGITS).unwrap_or(&item.tree);
        let name = format!("queue-{digits}");
        let dir = self.req.dir.join(&name);
        let mut gone = WsDeleteReq::new(name.clone());
        gone.force = true;
        match delete(self.ctx, gone) {
            Err(e) if e.code != ErrorCode::WsNotFound.as_str() => return Err(e),
            _ => {}
        }
        let mut req = WsCreateReq::new(name.clone());
        req.path = Some(dir.to_string());
        req.base = item.candidate.as_str().into();
        req.all = true;
        req.status_after = StatusAfter::None;
        create(self.ctx, req)?;
        Ok(Job {
            place,
            entry: item.entry.clone(),
            tree: item.tree.clone(),
            argv,
            name,
            dir,
        })
    }
}

/// What the store's verdict says of an item's prefix.
fn known(item: &QueueRoundItem) -> Known {
    known_of(item.verdict)
}

/// What a verdict says of its prefix: a cancel decides nothing.
fn known_of(verdict: Option<VerdictState>) -> Known {
    match verdict {
        Some(VerdictState::Passed) => Known::Green,
        Some(VerdictState::Failed | VerdictState::TimedOut) => Known::Red,
        _ => Known::Open,
    }
}

/// Why a red prefix's last entry is removed.
fn blame(verdict: Option<VerdictState>) -> RemovalReason {
    match verdict {
        Some(VerdictState::TimedOut) => RemovalReason::TimedOut,
        _ => RemovalReason::CheckFailed,
    }
}

/// The index in a chain of the prefix at `place`, from 1.
fn index(place: u32) -> usize {
    usize::try_from(place)
        .unwrap_or(usize::MAX)
        .saturating_sub(1)
}

/// The place, from 1, of the prefix at index `at` of a chain.
fn place(at: usize) -> u32 {
    u32::try_from(at + 1).unwrap_or(u32::MAX)
}

/// `E_IO` for the workspaces' directory `dir`, which could not be made.
fn host_failed(dir: &Utf8Path, e: &std::io::Error) -> OpError {
    OpError::new(ErrorCode::Io, format!("making {dir}: {e}")).with_details(Map::from_iter([(
        "file".to_owned(),
        Value::from(dir.as_str()),
    )]))
}
