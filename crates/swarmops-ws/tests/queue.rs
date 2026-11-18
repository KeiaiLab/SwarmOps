//! The landing queue's runner (the M22 core design §5, §6): `queue::enqueue`, the probes and
//! the rounds of `queue::run`, checked in real workspaces of a real store.
//!
//! The store's side of the queue is [`Queue`], a double of the queue facts held in memory over
//! the fixture's `LocalOps`: it builds each round's chain as real changesets on the target's head,
//! keeps the verdicts by tree, and moves the target only from the round's head to a tip whose tree
//! has a passed verdict, or answers `moved`. Its hooks put races where a test needs them.
//!
//! ```text
//!   test ──▶ queue::run(ctx with Queue) ──▶ Queue: lane, rounds, verdicts, publish (in memory)
//!                    │                                │
//!                    └─ workspaces, `sh -c CHECK` ───▶ LocalOps: changesets, the target
//! ```
//!
//! The check appends `start|end <tree> <uptime>` to a stamps file, and reads files of the
//! candidate's tree: `bad` fails it, `hang` runs past its timeout, `die` kills it by a signal,
//! `needs-fix` fails it unless `fixed` is there too, `wait-marker` blocks it until the marker
//! file exists.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use swarmops_api::{AncestryReq, AncestryResp, MergeBaseReq, MergeBaseResp};
use swarmops_api::{
    BlobGetReq, BlobGetResp, BlobHasReq, BlobHasResp, BlobPutReq, BlobPutResp, ChangeInput,
    ChangeOp, ChangesetDetail, CommitReq, CommitResp, ContentInput, DeriveReq, DeriveResp, Diff,
    DiffReq, ErrorCode, FileContent, Identity, InfoReq, LandReq, LandResp, LogPage, LogReq,
    ModeInput, OpError, OpLogPage, OpLogReq, PathHistoryPage, PathHistoryReq, QueueEnqueueReq,
    QueueEnqueueResp, QueueEntryOut, QueueEntryState, QueuePublishReq, QueuePublishResp,
    QueuePublishStatus, QueueRoundItem, QueueRoundReq, QueueRoundResp, QueueStatusReq,
    QueueStatusResp, QueueVerdictReq, QueueWithdrawReq, ReadFileReq, ReadTreeReq, RebaseReq,
    RebaseResp, RefDeleteReq, RefListReq, RefListResp, RefSetReq, RefSetResp, RemovalReason,
    RepoInfo, ResolveRevReq, ResolvedRev, ShowReq, StoreOps, TreeListing, UndoReq, UndoResp,
    VerdictState, VerifyReport, VerifyReq,
};
use swarmops_ws::probe::{self, End, Known};
use swarmops_ws::queue::{self, CANCEL_LIMIT, RunReq, RunResp};
use swarmops_ws::{Ctx, LaneGuard, QueueDriver};

use crate::fixture::{AGENT, Fixture, Put, tree_holds};

/// The target every test queues on: not `main`, which is protected from deletion.
const TARGET: &str = "trunk";

/// The check: stamps, then what the candidate's tree asks for. `STAMPS`, `MARKER` and `SLEEP`
/// are replaced by the setup.
const CHECK: &str = r#"
stamp() { echo "$1 $SWARMOPS_CHECK_TREE $(cut -d' ' -f1 /proc/uptime)" >> STAMPS; }
stamp start
if [ -e hang ]; then sleep 60; fi
if [ -e die ]; then kill -KILL $$; fi
if [ -e wait-marker ]; then
  i=0
  while [ ! -e MARKER ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done
fi
if [ -e slow ]; then sleep 1; fi
sleep SLEEP
stamp end
if [ -e bad ]; then exit 1; fi
if [ -e needs-fix ] && [ ! -e fixed ]; then exit 1; fi
exit 0
"#;

/// The check's pause in most tests.
const QUICK: &str = "0.05";

// ---- the store's side of the queue, in memory ----

/// What became of an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fate {
    /// Open.
    Open,
    /// Landed by a publish.
    Landed,
    /// Removed, and why.
    Removed(RemovalReason),
}

/// One entry of the double.
#[derive(Debug, Clone)]
struct Entry {
    id: String,
    head: String,
    puts: Vec<(String, String)>,
    fate: Fate,
    cancelled: u32,
}

/// What the double holds: the entries in enqueue order, the verdicts by tree, and what the
/// runner did.
#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    /// The files each change's head puts, by its changeset.
    changes: HashMap<String, Vec<(String, String)>>,
    /// The decisive verdicts, by tree.
    verdicts: HashMap<String, VerdictState>,
    /// Every verdict recorded: entry, tree, state.
    log: Vec<(String, String, VerdictState)>,
    /// The tree of each changeset the chains built.
    trees: HashMap<String, String>,
    /// The link each entry made on each parent: its changeset and tree, built once.
    links: HashMap<(String, String), (String, String)>,
    /// The tips the publishes moved the target to, in order.
    published: Vec<String>,
    /// Whether a runner holds the lane.
    lane: bool,
    /// The next publish fails as a runner killed before it would.
    crash_publish: bool,
}

impl State {
    /// Enqueues the change whose head is `head`.
    fn push(&mut self, head: &str) -> String {
        let id = format!("{:064x}", self.entries.len() + 1);
        let puts = self.changes[head].clone();
        self.entries.push(Entry {
            id: id.clone(),
            head: head.to_owned(),
            puts,
            fate: Fate::Open,
            cancelled: 0,
        });
        id
    }

    /// Entry `id`.
    fn entry(&mut self, id: &str) -> &mut Entry {
        (self.entries.iter_mut())
            .find(|entry| entry.id == id)
            .unwrap()
    }
}

/// A hook the double runs inside one of its calls, with the fixture and its state.
type Hook<'f> = Box<dyn FnMut(&Fixture, &mut State) + Send + 'f>;

/// A hook the double runs as its lane is released, once.
type OnRelease = Box<dyn FnOnce(&mut State) + Send>;

/// What the double and its lane guards share.
struct Shared {
    state: Mutex<State>,
    /// Runs when the lane is released, once.
    on_release: Mutex<Option<OnRelease>>,
}

/// The double: `StoreOps` over the fixture's store, with the queue operations its own, and the
/// runner's `QueueDriver`.
struct Queue<'f> {
    f: &'f Fixture,
    shared: Arc<Shared>,
    /// Runs after each verdict is recorded.
    after_verdict: Mutex<Option<Hook<'f>>>,
    /// Runs as a publish starts, before it compares anything.
    before_publish: Mutex<Option<Hook<'f>>>,
}

impl<'f> Queue<'f> {
    fn new(f: &'f Fixture) -> Queue<'f> {
        Queue {
            f,
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                on_release: Mutex::new(None),
            }),
            after_verdict: Mutex::new(None),
            before_publish: Mutex::new(None),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap()
    }

    /// The target's head; none when it is gone.
    fn head(&self) -> Option<String> {
        let resolved = self.f.ops.resolve_rev(ResolveRevReq::new(TARGET));
        resolved.ok().map(|resolved| resolved.id)
    }

    /// The fate of entry `id`.
    fn fate(&self, id: &str) -> Fate {
        self.state().entry(id).fate.clone()
    }

    /// The verdicts recorded on entry `id`.
    fn verdicts_of(&self, id: &str) -> Vec<VerdictState> {
        let state = self.state();
        (state.log.iter())
            .filter(|(entry, _, _)| entry == id)
            .map(|(_, _, verdict)| *verdict)
            .collect()
    }

    /// Commits `puts` on `parent` without moving a ref: its changeset and tree.
    fn link(&self, parent: &str, puts: &[(String, String)]) -> (String, String) {
        let changes = (puts.iter())
            .map(|(path, text)| {
                let mut out = ChangeInput::new(path.clone(), ChangeOp::Put);
                out.content = Some(ContentInput::Inline(text.clone()));
                out.mode = Some(ModeInput::File);
                out
            })
            .collect();
        let mut req = CommitReq::new(changes, "queue chain".to_owned());
        req.parent = Some(parent.into());
        let done = self.f.ops.commit(&who(), req).unwrap();
        (done.changeset, done.root)
    }
}

/// The lane of the double, released when dropped.
struct Lane(Arc<Shared>);

impl LaneGuard for Lane {}

impl Drop for Lane {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.lane = false;
        if let Some(hook) = self.0.on_release.lock().unwrap().take() {
            hook(&mut state);
        }
    }
}

impl QueueDriver for Queue<'_> {
    fn try_lane(&self, _target: &str) -> Result<Option<Box<dyn LaneGuard>>, OpError> {
        let mut state = self.state();
        if state.lane {
            return Ok(None);
        }
        state.lane = true;
        Ok(Some(Box::new(Lane(Arc::clone(&self.shared)))))
    }

    fn lane_free(&self, _target: &str) -> Result<bool, OpError> {
        Ok(!self.state().lane)
    }

    fn plan_round(&self, req: QueueRoundReq) -> Result<QueueRoundResp, OpError> {
        // 1. The target gone: every open entry removed.
        let mut out = QueueRoundResp::new(TARGET, Vec::new());
        let Some(head) = self.head() else {
            let mut state = self.state();
            for entry in state.entries.iter_mut().filter(|e| e.fate == Fate::Open) {
                entry.fate = Fate::Removed(RemovalReason::TargetGone);
                out.removed.push(entry.id.clone());
            }
            return Ok(out);
        };
        out.head = Some(head.clone());
        if req
            .expect_head
            .as_ref()
            .is_some_and(|expect| *expect != head)
        {
            return Ok(out);
        }

        // 2. The chain of the open entries on the head, each link a real changeset.
        let open: Vec<Entry> = (self.state().entries.iter())
            .filter(|entry| entry.fate == Fate::Open)
            .take(32)
            .cloned()
            .collect();
        let mut tip = head;
        for entry in open {
            let made = self
                .state()
                .links
                .get(&(tip.clone(), entry.id.clone()))
                .cloned();
            let (candidate, tree) = made.unwrap_or_else(|| self.link(&tip, &entry.puts));
            let mut state = self.state();
            let at = (tip.clone(), entry.id.clone());
            state.links.insert(at, (candidate.clone(), tree.clone()));
            state.trees.insert(candidate.clone(), tree.clone());
            let mut item = QueueRoundItem::new(entry.id.clone(), candidate.clone(), tree.clone());
            item.verdict = state.verdicts.get(&tree).copied();
            item.cancelled = entry.cancelled;
            out.items.push(item);
            tip = candidate;
        }
        Ok(out)
    }

    fn record_verdict(&self, req: QueueVerdictReq) -> Result<(), OpError> {
        // 1. Kept: a cancelled one counts on its entry, the others decide its tree.
        {
            let mut state = self.state();
            if req.state == VerdictState::Cancelled {
                state.entry(&req.entry).cancelled += 1;
            } else {
                state.verdicts.insert(req.tree.clone(), req.state);
            }
            state.log.push((req.entry, req.tree, req.state));
        }

        // 2. The test's race.
        if let Some(hook) = self.after_verdict.lock().unwrap().as_mut() {
            hook(self.f, &mut self.state());
        }
        Ok(())
    }

    fn publish(&self, req: QueuePublishReq) -> Result<QueuePublishResp, OpError> {
        // 1. The test's race, then a runner killed here.
        if let Some(hook) = self.before_publish.lock().unwrap().as_mut() {
            hook(self.f, &mut self.state());
        }
        let mut state = self.state();
        if std::mem::take(&mut state.crash_publish) {
            return Err(OpError::new(ErrorCode::Io, "killed before the publish"));
        }

        // 2. All or nothing: the head and every entry as the round saw them.
        let head = self.head();
        let named = (req.landed.iter()).chain(req.removed.iter().map(|removal| &removal.entry));
        let changed = named.clone().any(|id| state.entry(id).fate != Fate::Open);
        if head.as_deref() != Some(req.head.as_str()) || changed {
            let mut out = QueuePublishResp::new(QueuePublishStatus::Moved);
            out.head = head;
            return Ok(out);
        }

        // 3. The tip only to a tree with a passed verdict.
        if let Some(tip) = &req.tip {
            let tree = state.trees[tip].clone();
            assert_eq!(
                state.verdicts.get(&tree),
                Some(&VerdictState::Passed),
                "publish of {tip}, whose tree has no passed verdict"
            );
            let mut set = RefSetReq::new(TARGET, tip.as_str());
            set.expected_old = Some(req.head.as_str().into());
            self.f.ops.ref_set(&who(), set)?;
            state.published.push(tip.clone());
        }
        for id in &req.landed {
            state.entry(id).fate = Fate::Landed;
        }
        for removal in &req.removed {
            state.entry(&removal.entry).fate = Fate::Removed(removal.reason);
        }
        let mut out = QueuePublishResp::new(QueuePublishStatus::Published);
        out.head = self.head();
        Ok(out)
    }
}

impl StoreOps for Queue<'_> {
    fn queue_enqueue(
        &self,
        _who: &Identity,
        req: QueueEnqueueReq,
    ) -> Result<QueueEnqueueResp, OpError> {
        let head = self.f.ops.resolve_rev(ResolveRevReq::new(req.head))?.id;
        let id = self.state().push(&head);
        let entry = QueueEntryOut::new(id, TARGET, head, "", QueueEntryState::Waiting);
        Ok(QueueEnqueueResp::new(entry, false))
    }

    fn queue_status(&self, _req: QueueStatusReq) -> Result<QueueStatusResp, OpError> {
        let state = self.state();
        let entries = (state.entries.iter())
            .map(|entry| {
                let at = match entry.fate {
                    Fate::Open => QueueEntryState::Waiting,
                    Fate::Landed => QueueEntryState::Landed,
                    Fate::Removed(_) => QueueEntryState::Removed,
                };
                QueueEntryOut::new(entry.id.clone(), TARGET, entry.head.clone(), "", at)
            })
            .collect();
        Ok(QueueStatusResp::new(entries))
    }

    fn queue_withdraw(
        &self,
        _who: &Identity,
        req: QueueWithdrawReq,
    ) -> Result<QueueEntryOut, OpError> {
        let mut state = self.state();
        let entry = state.entry(&req.entry);
        entry.fate = Fate::Removed(RemovalReason::Withdrawn);
        let head = entry.head.clone();
        Ok(QueueEntryOut::new(
            req.entry,
            TARGET,
            head,
            "",
            QueueEntryState::Removed,
        ))
    }

    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError> {
        self.f.ops.repo_info(req)
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        self.f.ops.commit(who, req)
    }

    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError> {
        self.f.ops.land(who, req)
    }

    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError> {
        self.f.ops.rebase(who, req)
    }

    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError> {
        self.f.ops.resolve_rev(req)
    }

    fn blob_put(&self, who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError> {
        self.f.ops.blob_put(who, req)
    }

    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError> {
        self.f.ops.blob_has(req)
    }

    fn blob_get(&self, req: BlobGetReq) -> Result<BlobGetResp, OpError> {
        self.f.ops.blob_get(req)
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        self.f.ops.read_file(req)
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        self.f.ops.read_tree(req)
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        self.f.ops.show(req)
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        self.f.ops.log(req)
    }

    fn ancestry(&self, req: AncestryReq) -> Result<AncestryResp, OpError> {
        self.f.ops.ancestry(req)
    }

    fn path_history(&self, req: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
        self.f.ops.path_history(req)
    }

    fn diff(&self, req: DiffReq) -> Result<Diff, OpError> {
        self.f.ops.diff(req)
    }

    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError> {
        self.f.ops.ref_list(req)
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        self.f.ops.ref_set(who, req)
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        self.f.ops.ref_delete(who, req)
    }

    fn op_log(&self, req: OpLogReq) -> Result<OpLogPage, OpError> {
        self.f.ops.op_log(req)
    }

    fn undo(&self, who: &Identity, req: UndoReq) -> Result<UndoResp, OpError> {
        self.f.ops.undo(who, req)
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        self.f.ops.verify(req)
    }

    fn derive(&self, req: DeriveReq) -> Result<DeriveResp, OpError> {
        self.f.ops.derive(req)
    }

    fn merge_base(&self, req: MergeBaseReq) -> Result<MergeBaseResp, OpError> {
        self.f.ops.merge_base(req)
    }
}

// ---- the setup ----

/// The fixture's identity.
fn who() -> Identity {
    Identity::parse(AGENT).unwrap()
}

/// A store whose target holds the check, sleeping `pause` seconds, and where its stamps and
/// marker go.
struct Setup {
    f: Fixture,
    stamps: Utf8PathBuf,
    marker: Utf8PathBuf,
    base: String,
}

impl Setup {
    fn new(pause: &str) -> Setup {
        let f = Fixture::new();
        let stamps = f.beside("stamps");
        let marker = f.beside("marker");
        let script = CHECK
            .replace("STAMPS", stamps.as_str())
            .replace("MARKER", marker.as_str())
            .replace("SLEEP", pause);
        let config = serde_json::json!({ "check": { "argv": ["sh", "-c", script] } });
        let base = f.commit(
            None,
            &[
                Put::File(".swarmops/workspaces.json", &config.to_string()),
                Put::File("README", "queue\n"),
            ],
        );
        f.point(TARGET, &base);
        Setup {
            f,
            stamps,
            marker,
            base,
        }
    }

    /// A change on the base putting `name` and the files `flags` names, enqueued through
    /// `queue::enqueue`: its entry.
    fn enqueue(&self, ctx: &Ctx<'_>, q: &Queue<'_>, name: &str, flags: &[&str]) -> String {
        let head = self.change(q, name, flags);
        let req = QueueEnqueueReq::new(TARGET, head.as_str());
        queue::enqueue(ctx, req).unwrap().entry.id
    }

    /// A change on the base putting `name` and the files `flags` names, not enqueued: its
    /// head.
    fn change(&self, q: &Queue<'_>, name: &str, flags: &[&str]) -> String {
        let mut puts = vec![(name.to_owned(), format!("{name}\n"))];
        puts.extend(flags.iter().map(|flag| ((*flag).to_owned(), String::new())));
        let (head, _) = q.link(&self.base, &puts);
        q.state().changes.insert(head.clone(), puts);
        head
    }

    /// The runner's request: `width` checks at once, its workspaces beside the store.
    fn run_req(&self, width: u32) -> RunReq {
        let mut req = RunReq::new(TARGET, self.f.beside("queue-ws"));
        req.inflight = width;
        req.check_timeout = Duration::from_secs(20);
        req
    }

    /// The stamps the checks wrote: what, the tree, the uptime.
    fn stamps(&self) -> Vec<(String, String, f64)> {
        let Ok(text) = std::fs::read_to_string(&self.stamps) else {
            return Vec::new();
        };
        (text.lines())
            .map(|line| {
                let mut fields = line.split_whitespace();
                let what = fields.next().unwrap().to_owned();
                let tree = fields.next().unwrap().to_owned();
                let at = fields.next().unwrap().parse().unwrap();
                (what, tree, at)
            })
            .collect()
    }

    /// The trees whose check started, in order.
    fn started(&self) -> Vec<String> {
        (self.stamps().into_iter())
            .filter(|(what, _, _)| what == "start")
            .map(|(_, tree, _)| tree)
            .collect()
    }
}

/// The context of the runner over `q`.
fn ctx_of<'q, 'f: 'q>(s: &'q Setup, q: &'q Queue<'f>) -> Ctx<'q> {
    s.f.ctx_over(q).with_queue(q)
}

/// Runs the queue to its end.
fn drain(s: &Setup, q: &Queue<'_>, width: u32) -> RunResp {
    queue::run(&ctx_of(s, q), &s.run_req(width)).unwrap()
}

// ---- K11b ----

/// K11b: an enqueue whose head names no check is refused before the store is asked, and
/// nothing is written.
#[test]
fn enqueue_without_check_is_refused() {
    // 1. A store whose head holds no `.swarmops/workspaces.json`.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("README", "no check\n")]);
    let head = f.side(Some(&base), &[Put::File("a", "a\n")]);
    let before = f.last_op();

    // 2. Refused with the missing check, as `ws land --check` is.
    let req = QueueEnqueueReq::new(TARGET, head.as_str());
    let err = queue::enqueue(&f.ctx(), req).unwrap_err();
    assert_eq!(err.code, "E_INVALID_ARG", "{err:?}");
    assert_eq!(err.details["missing"], "check", "{err:?}");

    // 3. No operation recorded: the store's queue was never reached.
    assert_eq!(f.last_op(), before);
}

// ---- K12 ----

/// K12: the probes split the undecided interval: all of it when it fits, spaced ones ending at
/// its end while that is undecided, inner ones once it is red; with one at a time a bisection.
#[test]
fn probes_split_the_undecided_interval() {
    // 1. All of (g, r] when it fits the width.
    assert_eq!(probe::select(0, 4, End::Open, 4), [1, 2, 3, 4]);
    assert_eq!(probe::select(2, 5, End::Open, 4), [3, 4, 5]);

    // 2. Wider: w spaced ones ending at r.
    assert_eq!(probe::select(0, 8, End::Open, 4), [2, 4, 6, 8]);
    assert_eq!(
        probe::select(0, 32, End::Open, 8),
        [4, 8, 12, 16, 20, 24, 28, 32]
    );
    assert_eq!(probe::select(3, 10, End::Open, 2), [7, 10]);

    // 3. r red: w inside (g, r), none twice, none at r.
    assert_eq!(probe::select(0, 8, End::Red, 3), [2, 4, 6]);
    assert_eq!(probe::select(0, 3, End::Red, 4), [1, 2]);
    assert_eq!(probe::select(4, 5, End::Red, 4), Vec::<u32>::new());

    // 4. One at a time: the middle, a binary search down to the red entry.
    assert_eq!(probe::select(0, 8, End::Open, 1), [8]);
    assert_eq!(probe::select(0, 8, End::Red, 1), [4]);
    assert_eq!(probe::select(4, 8, End::Red, 1), [6]);
    assert_eq!(probe::select(4, 6, End::Red, 1), [5]);

    // 5. The interval the verdicts leave.
    let (o, g, r) = (Known::Open, Known::Green, Known::Red);
    let found = probe::decide(&[g, o, g, o, r, r]);
    assert_eq!((found.green, found.end, found.red), (3, 5, End::Red));
    let found = probe::decide(&[o, g, o, o]);
    assert_eq!((found.green, found.end, found.red), (2, 4, End::Open));
    let found = probe::decide(&[r, g]);
    assert_eq!((found.green, found.end, found.red), (0, 1, End::Red));
}

// ---- K13 ----

/// K13: four entries checked at once, the third red: the first two land in one publish, the
/// third is removed naming its candidate tree, the fourth lands next round.
#[test]
fn a_red_third_lands_the_green_prefix() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let ids = [
        s.enqueue(&ctx, &q, "e1", &[]),
        s.enqueue(&ctx, &q, "e2", &[]),
        s.enqueue(&ctx, &q, "e3", &["bad"]),
        s.enqueue(&ctx, &q, "e4", &[]),
    ];

    let out = drain(&s, &q, 4);

    // 1. Two publishes: entries 1-2, then 4.
    let published = q.state().published.clone();
    assert_eq!(published.len(), 2, "{out:?}");
    assert!(tree_holds(&s.f, &published[0], "e2"));
    assert!(!tree_holds(&s.f, &published[0], "e3"));
    assert!(tree_holds(&s.f, &published[1], "e4"));
    assert!(!tree_holds(&s.f, &published[1], "bad"));

    // 2. Entry 3 removed for its check, the red verdict on its own prefix's tree.
    assert_eq!(q.fate(&ids[0]), Fate::Landed);
    assert_eq!(q.fate(&ids[1]), Fate::Landed);
    assert_eq!(q.fate(&ids[2]), Fate::Removed(RemovalReason::CheckFailed));
    assert_eq!(q.fate(&ids[3]), Fate::Landed);
    assert_eq!(q.verdicts_of(&ids[2]), [VerdictState::Failed]);
}

// ---- K14 ----

/// K14: one check at a time: eight green entries take one check and one move; one red among
/// eight is found by bisection in at most seven checks, and the other seven land.
#[test]
fn batch_of_eight_bisects_one_red() {
    // 1. Eight green: the whole chain checked once, moved once.
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    for i in 1..=8 {
        s.enqueue(&ctx, &q, &format!("e{i}"), &[]);
    }
    let out = drain(&s, &q, 1);
    assert_eq!(out.checks, 1, "{out:?}");
    assert_eq!(q.state().published.len(), 1);
    assert_eq!(s.started().len(), 1);

    // 2. One red, the fifth: at most seven checks, only it removed.
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let ids: Vec<String> = (1..=8)
        .map(|i| {
            let flags: &[&str] = if i == 5 { &["bad"] } else { &[] };
            s.enqueue(&ctx, &q, &format!("e{i}"), flags)
        })
        .collect();
    let out = drain(&s, &q, 1);
    assert!(out.checks <= 7, "{out:?}");
    assert_eq!(s.started().len(), usize::try_from(out.checks).unwrap());
    for (i, id) in ids.iter().enumerate() {
        let want = if i == 4 {
            Fate::Removed(RemovalReason::CheckFailed)
        } else {
            Fate::Landed
        };
        assert_eq!(q.fate(id), want, "entry {}", i + 1);
    }
}

// ---- K15 ----

/// K15: 32 entries, eight checks at once, each 300 ms: all land within four rounds, the ref
/// moving forward in queue order, eight checks overlapping, in about four checks' time.
#[test]
fn thirty_two_entries_land_in_four_rounds() {
    const PAUSE: f64 = 0.3;
    const SLACK_S: f64 = 10.0;
    let s = Setup::new("0.3");
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let ids: Vec<String> = (1..=32)
        .map(|i| s.enqueue(&ctx, &q, &format!("e{i}"), &[]))
        .collect();

    // 1. Drained in at most four rounds of checks.
    let out = drain(&s, &q, 8);
    assert!(out.rounds <= 4, "{out:?}");

    // 2. Every entry landed, and the final tree holds every change.
    for id in &ids {
        assert_eq!(q.fate(id), Fate::Landed);
    }
    let head = q.head().unwrap();
    for i in 1..=32 {
        assert!(tree_holds(&s.f, &head, &format!("e{i}")), "e{i}");
    }

    // 3. Forward in queue order: each published tip holds a longer prefix of the entries.
    let published = q.state().published.clone();
    let mut landed = 0;
    for tip in &published {
        let prefix = (1..=32)
            .take_while(|i| tree_holds(&s.f, tip, &format!("e{i}")))
            .count();
        assert!(
            prefix > landed,
            "{tip} holds {prefix} entries, after {landed}"
        );
        let gap = tree_holds(&s.f, tip, &format!("e{}", prefix + 1));
        assert!(!gap || prefix == 32, "{tip} skips entry {}", prefix + 1);
        landed = prefix;
    }
    assert_eq!(landed, 32);

    // 4. At some instant eight checks ran at once, and the checks took about four checks'
    //    time from the first start to the last end.
    let stamps = s.stamps();
    let mut events: Vec<(f64, i32)> = (stamps.iter())
        .map(|(what, _, at)| (*at, if what == "start" { 1 } else { -1 }))
        .collect();
    events.sort_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut running = 0;
    let mut most = 0;
    for (_, step) in &events {
        running += step;
        most = most.max(running);
    }
    assert!(most >= 8, "at most {most} checks overlapped");
    let took = events.last().unwrap().0 - events[0].0;
    assert!(took <= 4.0 * PAUSE + SLACK_S, "the checks took {took} s");
}

// ---- K16 ----

/// K16: entries that arrive while checks run restart none of them, and after a publish of a
/// prefix the verdicts of the later probes are reused by tree: each tree is checked once.
#[test]
fn arrivals_and_own_lands_cause_no_rechecks() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);

    // 1. Eight entries, the sixth red; two more arrive at the first verdict.
    let ids: Vec<String> = (1..=8)
        .map(|i| {
            let flags: &[&str] = if i == 6 { &["bad"] } else { &[] };
            s.enqueue(&ctx, &q, &format!("e{i}"), flags)
        })
        .collect();
    let late = [s.change(&q, "late1", &[]), s.change(&q, "late2", &[])];
    let mut arrived = false;
    *q.after_verdict.lock().unwrap() = Some(Box::new(move |_, state: &mut State| {
        if !std::mem::replace(&mut arrived, true) {
            for head in &late {
                state.push(head);
            }
        }
    }));

    let out = drain(&s, &q, 4);

    // 2. No tree checked twice.
    let started = s.started();
    let mut distinct = started.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(started.len(), distinct.len(), "{started:?}");
    assert_eq!(out.checks as usize, started.len());

    // 3. All but the red one landed, the arrivals too.
    let state = q.state();
    assert_eq!(state.entries.len(), 10);
    for (i, entry) in state.entries.iter().enumerate() {
        let want = if entry.id == ids[5] {
            Fate::Removed(RemovalReason::CheckFailed)
        } else {
            Fate::Landed
        };
        assert_eq!(entry.fate, want, "entry {}", i + 1);
    }
}

// ---- K17 ----

/// K17: a runner killed after its verdicts were recorded, before its publish: the next one
/// checks no passed tree again and lands the queue.
#[test]
fn a_restarted_runner_reuses_passed_verdicts() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let ids: Vec<String> = (1..=4)
        .map(|i| s.enqueue(&ctx, &q, &format!("e{i}"), &[]))
        .collect();

    // 1. Killed at its publish.
    q.state().crash_publish = true;
    let err = queue::run(&ctx, &s.run_req(4)).unwrap_err();
    assert_eq!(err.code, "E_IO", "{err:?}");
    let checked = s.started().len();
    assert!(checked > 0);
    assert_eq!(q.state().published.len(), 0);

    // 2. The next runner checks nothing and lands all four.
    let out = drain(&s, &q, 4);
    assert_eq!(out.checks, 0, "{out:?}");
    assert_eq!(s.started().len(), checked);
    for id in &ids {
        assert_eq!(q.fate(id), Fate::Landed);
    }
}

// ---- K18 ----

/// K18: the published tree is a checked one. The target moves while entry 1's check runs, its
/// check red only for want of the file the move brings: the publish lands nothing, entry 1 is
/// not removed, and the next round on the new head checks it again and lands it.
#[test]
fn the_published_tree_is_the_checked_tree() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let first = s.enqueue(&ctx, &q, "e1", &["needs-fix", "wait-marker"]);
    let second = s.enqueue(&ctx, &q, "e2", &[]);

    // 1. While the first check runs: the target moves outside the queue, then the check goes
    //    on.
    let out = std::thread::scope(|scope| {
        scope.spawn(|| {
            wait_for_start(&s);
            let head = q.head().unwrap();
            let fixed = s.f.side(Some(&head), &[Put::File("fixed", "\n")]);
            s.f.point(TARGET, &fixed);
            std::fs::write(&s.marker, "").unwrap();
        });
        drain(&s, &q, 4)
    });

    // 2. Entry 1 rebuilt on the new head and landed, entry 2 too; never removed.
    assert_eq!(q.fate(&first), Fate::Landed, "{out:?}");
    assert_eq!(q.fate(&second), Fate::Landed);
    assert_eq!(q.verdicts_of(&first)[0], VerdictState::Failed);

    // 3. Every tip published has a passed verdict, and the target stands on the last.
    let state = q.state();
    for tip in &state.published {
        let tree = &state.trees[tip];
        assert_eq!(state.verdicts.get(tree), Some(&VerdictState::Passed));
    }
    let head = q.head().unwrap();
    assert_eq!(state.published.last(), Some(&head));
    assert!(tree_holds(&s.f, &head, "fixed"));
}

/// Waits until a check has started.
fn wait_for_start(s: &Setup) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while s.started().is_empty() {
        assert!(Instant::now() < deadline, "no check started");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- K19 ----

/// K19: a check killed from outside is cancelled, blames no one at first, and the third
/// cancelled verdict removes its entry; a check past its timeout removes its entry as
/// timed out.
#[test]
fn an_external_kill_is_not_blamed() {
    // 1. Killed by a signal, every time: three cancelled verdicts, then removed; the entry
    //    behind it counts none and lands.
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let killed = s.enqueue(&ctx, &q, "e1", &["die"]);
    let behind = s.enqueue(&ctx, &q, "e2", &[]);
    drain(&s, &q, 4);
    assert_eq!(
        q.verdicts_of(&killed),
        [VerdictState::Cancelled; CANCEL_LIMIT as usize]
    );
    assert_eq!(q.fate(&killed), Fate::Removed(RemovalReason::Cancelled));
    assert!(
        !q.verdicts_of(&behind).contains(&VerdictState::Cancelled),
        "{:?}",
        q.verdicts_of(&behind)
    );
    assert_eq!(q.fate(&behind), Fate::Landed);

    // 2. Past the timeout: removed as timed out at once.
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let hung = s.enqueue(&ctx, &q, "e1", &["hang"]);
    let mut req = s.run_req(4);
    req.check_timeout = Duration::from_secs(1);
    queue::run(&ctx, &req).unwrap();
    assert_eq!(q.verdicts_of(&hung), [VerdictState::TimedOut]);
    assert_eq!(q.fate(&hung), Fate::Removed(RemovalReason::TimedOut));
}

// ---- K19b ----

/// K19b: the check of the entry after a red one still runs when its prefix is decided, three
/// rounds over: the runner's own cancel counts nothing against it.
#[test]
fn a_runner_cancel_is_not_counted() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    for round in 1..=3 {
        // 1. Green, green, red, and a slow fourth.
        s.enqueue(&ctx, &q, &format!("r{round}e1"), &[]);
        s.enqueue(&ctx, &q, &format!("r{round}e2"), &[]);
        let red = s.enqueue(&ctx, &q, &format!("r{round}e3"), &["bad"]);
        let slow = s.enqueue(&ctx, &q, &format!("r{round}e4"), &["slow"]);
        drain(&s, &q, 4);

        // 2. The red one removed, the slow one never cancelled.
        assert_eq!(q.fate(&red), Fate::Removed(RemovalReason::CheckFailed));
        assert!(!q.verdicts_of(&slow).contains(&VerdictState::Cancelled));
        assert_eq!(q.fate(&slow), Fate::Landed, "round {round}");
    }
}

// ---- K20 ----

/// K20: an entry enqueued as the runner releases its lane lands without another command.
#[test]
fn an_entry_enqueued_at_lane_release_is_landed() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let first = s.enqueue(&ctx, &q, "e1", &[]);
    let late = s.change(&q, "late", &[]);

    // 1. The release enqueues it, as an enqueue racing the release would.
    *q.shared.on_release.lock().unwrap() = Some(Box::new(move |state: &mut State| {
        state.push(&late);
    }));
    drain(&s, &q, 4);

    // 2. Both landed by the one runner.
    let state = q.state();
    assert_eq!(state.entries.len(), 2);
    assert!(state.entries.iter().all(|e| e.fate == Fate::Landed));
    drop(state);
    assert_eq!(q.fate(&first), Fate::Landed);
}

// ---- K21 ----

/// K21: no store lock spans a check: another writer commits while the check waits for a
/// marker made only after that commit returns.
#[test]
fn no_store_lock_is_held_during_a_check() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let id = s.enqueue(&ctx, &q, "e1", &["wait-marker"]);

    // 1. The other writer, then the marker.
    let committed = std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            wait_for_start(&s);
            let mut put = ChangeInput::new("other".to_owned(), ChangeOp::Put);
            put.content = Some(ContentInput::Inline("writer\n".to_owned()));
            let mut req = CommitReq::new(vec![put], "another writer".to_owned());
            req.parent = Some(s.base.as_str().into());
            let done = s.f.ops.commit(&who(), req);
            std::fs::write(&s.marker, "").unwrap();
            done.is_ok()
        });
        drain(&s, &q, 4);
        writer.join().unwrap()
    });

    // 2. The commit went through during the check, and the entry landed.
    assert!(committed, "the commit failed while the check ran");
    assert_eq!(q.fate(&id), Fate::Landed);
}

// ---- K22 ----

/// K22: an entry withdrawn after its prefix was decided is in no published tree: that publish
/// lands nothing, and the next round lands the others without it.
#[test]
fn a_withdrawn_entry_is_never_published() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let ids = [
        s.enqueue(&ctx, &q, "e1", &[]),
        s.enqueue(&ctx, &q, "e2", &[]),
        s.enqueue(&ctx, &q, "e3", &[]),
    ];

    // 1. Withdrawn as the first publish starts.
    let withdrawn = ids[1].clone();
    let mut once = false;
    *q.before_publish.lock().unwrap() = Some(Box::new(move |_, state: &mut State| {
        if !std::mem::replace(&mut once, true) {
            state.entry(&withdrawn).fate = Fate::Removed(RemovalReason::Withdrawn);
        }
    }));
    drain(&s, &q, 4);

    // 2. The others landed, in trees without it.
    assert_eq!(q.fate(&ids[0]), Fate::Landed);
    assert_eq!(q.fate(&ids[1]), Fate::Removed(RemovalReason::Withdrawn));
    assert_eq!(q.fate(&ids[2]), Fate::Landed);
    let published = q.state().published.clone();
    assert_ne!(published.len(), 0);
    for tip in &published {
        assert!(
            !tree_holds(&s.f, tip, "e2"),
            "{tip} holds the withdrawn change"
        );
    }
}

// ---- K23 ----

/// K23: the target deleted: every open entry is removed as `target_gone`.
#[test]
fn a_deleted_target_removes_its_entries() {
    let s = Setup::new(QUICK);
    let q = Queue::new(&s.f);
    let ctx = ctx_of(&s, &q);
    let ids = [
        s.enqueue(&ctx, &q, "e1", &[]),
        s.enqueue(&ctx, &q, "e2", &[]),
    ];
    let req = RefDeleteReq::new(TARGET, q.head().unwrap().as_str());
    s.f.ops.ref_delete(&who(), req).unwrap();

    let out = drain(&s, &q, 4);

    assert_eq!(out.checks, 0, "{out:?}");
    for id in &ids {
        assert_eq!(q.fate(id), Fate::Removed(RemovalReason::TargetGone));
    }
}
