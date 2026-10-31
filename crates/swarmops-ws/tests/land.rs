//! `ws land`: without a check, the store's land of the workspace's chain; with one, rebase,
//! check and an exact land, again when the target moved, under one land key that a killed run
//! replays.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use camino::Utf8Path;
use swarmops_api::{
    BlobGetReq, BlobGetResp, BlobHasReq, BlobHasResp, BlobPutReq, BlobPutResp, ChangesetDetail,
    CommitReq, CommitResp, DeriveReq, DeriveResp, Diff, DiffReq, ErrorCode, FileContent, Identity,
    InfoReq, LandMode, LandReq, LandResp, LandStatus, LogPage, LogReq, OpError, OpLogPage,
    OpLogReq, PathHistoryPage, PathHistoryReq, ReadFileReq, ReadTreeReq, RebaseReq, RebaseResp,
    RebaseStatus, RefDeleteReq, RefListReq, RefListResp, RefSetReq, RefSetResp, RepoInfo,
    ResolveRevReq, ResolvedRev, ShowReq, StatusAfter, StoreOps, TreeListing, UndoReq, UndoResp,
    VerifyReport, VerifyReq, WsCheckInput, WsCommitReq, WsLandReq, WsRebaseAction, WsRebaseReq,
};
use swarmops_ws::state::{self, SWARMWS_DIR, Volume};
use swarmops_ws::status::BEHIND_CAP as HISTORY_CAP;
use swarmops_ws::{RealFs, lock};

use crate::fixture::{BARE, Fixture, Put, Watched, changes, file, tree_of, write};

/// The store calls an outside refusal makes: the workspace's open and the reads of its tree.
const OPEN_AND_READ: [&str; 2] = ["repo_info", "read_tree"];

/// A store that forwards every call to `inner`, and can run a hook before the first `land` (a
/// second process landing first) or fail right after a `land` or a `rebase` the store recorded,
/// as a process killed there would.
struct Hooked<'a> {
    /// The store the calls go to.
    inner: &'a dyn StoreOps,
    /// Run once, before the first `land` reaches the store.
    before_land: Mutex<Option<Box<dyn FnOnce() + Send + 'a>>>,
    /// Run once, before the first `rebase` reaches the store.
    before_rebase: Mutex<Option<Box<dyn FnOnce() + Send + 'a>>>,
    /// The next `land` fails before the store sees it.
    crash_before_land: AtomicBool,
    /// The next `land` is recorded, then fails.
    crash_after_land: AtomicBool,
    /// The next `rebase` is recorded, then fails.
    crash_after_rebase: AtomicBool,
    /// What each `land` the store answered gave.
    lands: Mutex<Vec<LandResp>>,
    /// What each `rebase` the store answered gave.
    rebases: Mutex<Vec<RebaseResp>>,
}

impl<'a> Hooked<'a> {
    /// A store forwarding to `inner`, with no hook and no crash.
    fn new(inner: &'a dyn StoreOps) -> Hooked<'a> {
        Hooked {
            inner,
            before_land: Mutex::new(None),
            before_rebase: Mutex::new(None),
            crash_before_land: AtomicBool::new(false),
            crash_after_land: AtomicBool::new(false),
            crash_after_rebase: AtomicBool::new(false),
            lands: Mutex::new(Vec::new()),
            rebases: Mutex::new(Vec::new()),
        }
    }

    /// The `land` answers so far.
    fn lands(&self) -> Vec<LandResp> {
        self.lands.lock().unwrap().clone()
    }

    /// The `rebase` answers so far.
    fn rebases(&self) -> Vec<RebaseResp> {
        self.rebases.lock().unwrap().clone()
    }
}

/// The error a killed process leaves its caller with.
fn killed() -> OpError {
    OpError::new(ErrorCode::Io, "killed after the store's frame")
}

impl StoreOps for Hooked<'_> {
    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError> {
        self.inner.repo_info(req)
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        self.inner.commit(who, req)
    }

    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError> {
        // 1. The hook first: another process lands before this one.
        let hook = self.before_land.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }

        // 2. A crash before the store, if asked for; else the land, recorded, and a crash after it.
        if self.crash_before_land.swap(false, Ordering::SeqCst) {
            return Err(killed());
        }
        let done = self.inner.land(who, req)?;
        self.lands.lock().unwrap().push(done.clone());
        if self.crash_after_land.swap(false, Ordering::SeqCst) {
            return Err(killed());
        }
        Ok(done)
    }

    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError> {
        let hook = self.before_rebase.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        let done = self.inner.rebase(who, req)?;
        self.rebases.lock().unwrap().push(done.clone());
        if self.crash_after_rebase.swap(false, Ordering::SeqCst) {
            return Err(killed());
        }
        Ok(done)
    }

    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError> {
        self.inner.resolve_rev(req)
    }

    fn blob_put(&self, who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError> {
        self.inner.blob_put(who, req)
    }

    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError> {
        self.inner.blob_has(req)
    }

    fn blob_get(&self, req: BlobGetReq) -> Result<BlobGetResp, OpError> {
        self.inner.blob_get(req)
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        self.inner.read_file(req)
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        self.inner.read_tree(req)
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        self.inner.show(req)
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        self.inner.log(req)
    }

    fn path_history(&self, req: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
        self.inner.path_history(req)
    }

    fn diff(&self, req: DiffReq) -> Result<Diff, OpError> {
        self.inner.diff(req)
    }

    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError> {
        self.inner.ref_list(req)
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        self.inner.ref_set(who, req)
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        self.inner.ref_delete(who, req)
    }

    fn op_log(&self, req: OpLogReq) -> Result<OpLogPage, OpError> {
        self.inner.op_log(req)
    }

    fn undo(&self, who: &Identity, req: UndoReq) -> Result<UndoResp, OpError> {
        self.inner.undo(who, req)
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        self.inner.verify(req)
    }

    fn derive(&self, req: DeriveReq) -> Result<DeriveResp, OpError> {
        self.inner.derive(req)
    }
}

/// The request of `ws land` of workspace `name`, with the check `argv` if any.
fn land_of(name: &str, argv: Option<&[String]>) -> WsLandReq {
    let mut out = WsLandReq::new(name.to_owned());
    out.check = argv.map(|argv| {
        let mut check = WsCheckInput::default();
        check.argv = Some(argv.to_vec());
        check.timeout_s = 900;
        check
    });
    out.max_attempts = 8;
    out.status_after = StatusAfter::Page;
    out
}

/// A shell command: `sh -c script sh args…`.
fn sh(script: &str, args: &[&str]) -> Vec<String> {
    let mut command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script.to_owned(),
        "sh".to_owned(),
    ];
    command.extend(args.iter().map(|arg| (*arg).to_owned()));
    command
}

/// A check that appends the tree it saw, its attempt and its workspace to the file `seen`.
fn recording(seen: &Utf8Path) -> Vec<String> {
    let script = "printf '%s %s %s\\n' \"$SWARMOPS_CHECK_TREE\" \"$SWARMOPS_CHECK_ATTEMPT\" \"$SWARMOPS_WS\" >> \"$1\"";
    sh(script, &[seen.as_str()])
}

/// The lines the check wrote to `seen`, each `[tree, attempt, workspace]`.
fn seen_lines(seen: &Utf8Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(seen).unwrap_or_default();
    (text.lines())
        .map(|line| line.split(' ').map(str::to_owned).collect())
        .collect()
}

/// The head of `main`.
fn main_of(f: &Fixture) -> String {
    let req = ResolveRevReq::new("main");
    f.ops.resolve_rev(req).unwrap().id
}

/// Changeset `id`, shown.
fn show(f: &Fixture, id: &str) -> ChangesetDetail {
    let mut req = ShowReq::new(id);
    req.max_changes = 1;
    f.ops.show(req).unwrap()
}

/// A workspace `w` on a base holding `src/a.txt`, with a commit of its own changing it, and main
/// moved since by a commit adding `src/b.txt`. The workspace's head and main's head.
fn behind(f: &Fixture) -> (String, String) {
    // 1. The base, and a workspace of it with one commit.
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &[]);
    write(&f.beside("w"), "src/a.txt", "mine\n");
    let mut req = WsCommitReq::new("w".to_owned(), "mine".to_owned());
    req.status_after = StatusAfter::None;
    let head = swarmops_ws::commit(&f.ctx(), req).unwrap().changeset;

    // 2. Main moves.
    let main = f.commit(Some(&base), &[Put::File("src/b.txt", "b\n")]);
    (head, main)
}

/// The pending record's path in workspace `dir`.
fn pending_in(dir: &Utf8Path) -> camino::Utf8PathBuf {
    dir.join(SWARMWS_DIR).join("journal/pending.json")
}

/// `ws land --check` rebases onto main, runs the check in the workspace with the rebased tree's
/// id in its environment, and lands that tree: `L`'s root is the tree the check saw.
#[cfg(unix)]
#[test]
fn ws_land_check_lands_the_tree_the_check_ran_on() {
    // 1. A workspace behind main.
    let f = Fixture::new();
    let (_, main) = behind(&f);
    let seen = f.beside("seen");

    // 2. Landed with a check.
    let done = swarmops_ws::land(&f.ctx(), land_of("w", Some(&recording(&seen)))).unwrap();
    assert_eq!(done.attempts, 1);
    assert_eq!(done.land.status, LandStatus::Landed);
    assert_eq!(done.land.rebased_onto, main);
    assert_eq!(done.status_after.unwrap().changes, []);

    // 3. The check ran once, in workspace w; L's root is the tree it saw.
    let lines = seen_lines(&seen);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0][1..], ["1", "w"]);
    let landed = show(&f, &done.land.landed);
    assert_eq!(landed.root, lines[0][0]);
    assert_eq!(main_of(&f), done.land.landed);

    // 4. The workspace stands on L, its files main's and its own, the check's log kept.
    let dir = f.beside("w");
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(
        (&saved.base, &saved.head),
        (&done.land.landed, &done.land.landed)
    );
    assert_eq!(saved.materialized, done.land.landed);
    assert_eq!(
        tree_of(&dir),
        [file("src/a.txt", "mine\n"), file("src/b.txt", "b\n")]
    );
    assert!(dir.join(SWARMWS_DIR).join("checks/1.log").exists());
    assert!(!pending_in(&dir).exists());
}

/// A second process lands between the check and the land: the exact land refuses the moved
/// target, the workspace rebases onto it and checks again, and the tree landed is the second
/// check's.
#[cfg(unix)]
#[test]
fn ws_land_check_rechecks_after_the_target_moves() {
    // 1. A workspace behind main, and a store where another land comes first.
    let f = Fixture::new();
    let (_, main) = behind(&f);
    let seen = f.beside("seen");
    let store = Hooked::new(&f.ops);
    let racer = main.clone();
    let fixture = &f;
    *store.before_land.lock().unwrap() = Some(Box::new(move || {
        fixture.commit(Some(&racer), &[Put::File("src/c.txt", "c\n")]);
    }));

    // 2. Landed with a check, in two attempts.
    let ctx = f.ctx_over(&store);
    let done = swarmops_ws::land(&ctx, land_of("w", Some(&recording(&seen)))).unwrap();
    assert_eq!(done.attempts, 2);

    // 3. The check ran twice, on two trees; L's root is the second.
    let lines = seen_lines(&seen);
    let attempts: Vec<&str> = lines.iter().map(|line| line[1].as_str()).collect();
    assert_eq!(attempts, ["1", "2"]);
    assert_ne!(lines[0][0], lines[1][0]);
    let landed = show(&f, &done.land.landed);
    assert_eq!(landed.root, lines[1][0]);

    // 4. One land on main, on the racer's changeset; the workspace holds all three files.
    let racer_head = &landed.parents[0];
    assert_eq!(show(&f, racer_head).parents, [main]);
    assert_eq!(main_of(&f), done.land.landed);
    assert_eq!(
        tree_of(&f.beside("w")),
        [
            file("src/a.txt", "mine\n"),
            file("src/b.txt", "b\n"),
            file("src/c.txt", "c\n"),
        ]
    );
}

/// A check that fails lands nothing: `E_CHECK_FAILED` with its exit code, the tails of its
/// output, its log and the changeset it ran on; the workspace stays rebased on that changeset.
#[cfg(unix)]
#[test]
fn a_failed_check_lands_nothing_and_leaves_the_workspace_rebased() {
    // 1. A workspace behind main, and a check that fails.
    let f = Fixture::new();
    let (head, main) = behind(&f);
    let failing = sh("echo out; echo err >&2; exit 3", &[]);

    // 2. Refused, with what the check did.
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", Some(&failing))).unwrap_err();
    assert_eq!(refused.code, "E_CHECK_FAILED", "{refused:?}");
    let details = &refused.details;
    assert_eq!(details["stage"], "check");
    assert_eq!(details["exit_code"], 3);
    assert_eq!(details["timed_out"], false);
    assert_eq!(details["stdout_tail"], "out\n");
    assert_eq!(details["stderr_tail"], "err\n");
    assert_eq!(details["log"], ".swarmws/checks/1.log");

    // 3. Main did not move; the workspace stands on H', rebased onto main.
    assert_eq!(main_of(&f), main);
    let dir = f.beside("w");
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(details["changeset"], saved.head.as_str());
    assert_ne!(saved.head, head);
    assert_eq!(show(&f, &saved.head).parents, std::slice::from_ref(&main));
    assert_eq!(
        (saved.base.as_str(), saved.onto.as_deref()),
        (&*main, Some(&*main))
    );
    assert_eq!(
        tree_of(&dir),
        [file("src/a.txt", "mine\n"), file("src/b.txt", "b\n")]
    );
    assert!(!pending_in(&dir).exists());
}

/// A check past its timeout is killed and is `E_CHECK_FAILED` with `timed_out` and no exit code.
#[cfg(unix)]
#[test]
fn a_check_past_its_timeout_is_e_check_failed_with_timed_out() {
    // 1. A workspace behind main, and a check that outlives its one second.
    let f = Fixture::new();
    behind(&f);
    let mut req = land_of("w", Some(&["sleep".to_owned(), "30".to_owned()]));
    req.check.as_mut().unwrap().timeout_s = 1;

    // 2. Killed at its timeout, not at its end.
    let started = std::time::Instant::now();
    let refused = swarmops_ws::land(&f.ctx(), req).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(refused.code, "E_CHECK_FAILED", "{refused:?}");
    assert_eq!(refused.details["timed_out"], true);
    assert!(
        refused
            .details
            .get("exit_code")
            .is_none_or(serde_json::Value::is_null)
    );
}

/// A `ws land --check` killed after the land's frame, before `workspace.json`, and one killed
/// after an attempt's rebase frame: each re-run takes the pending record's keys, so the store
/// replays the same `H'` and the same land while main has not moved, and main holds one land.
#[cfg(unix)]
#[test]
fn a_killed_ws_land_resumes_by_its_key() {
    // 1. Killed after the land: the re-run replays it without checking again.
    let f = Fixture::new();
    let (_, main) = behind(&f);
    let seen = f.beside("seen");
    let check = recording(&seen);
    let store = Hooked::new(&f.ops);
    store.crash_after_land.store(true, Ordering::SeqCst);
    let ctx = f.ctx_over(&store);
    swarmops_ws::land(&ctx, land_of("w", Some(&check))).unwrap_err();
    let first = store.lands()[0].landed.clone();
    let dir = f.beside("w");
    assert!(pending_in(&dir).exists());
    let done = swarmops_ws::land(&ctx, land_of("w", Some(&check))).unwrap();
    assert_eq!(done.land.status, LandStatus::Replayed);
    assert_eq!(done.land.landed, first);
    assert_eq!(seen_lines(&seen).len(), 1);
    assert_eq!(
        (main_of(&f), show(&f, &first).parents),
        (first.clone(), vec![main])
    );
    assert_eq!(state::read(&RealFs::new(), &dir).unwrap().head, first);
    assert!(!pending_in(&dir).exists());

    // 2. Killed after the rebase: the re-run replays the same H', then lands it once.
    let g = Fixture::new();
    let (head, main) = behind(&g);
    let store = Hooked::new(&g.ops);
    store.crash_after_rebase.store(true, Ordering::SeqCst);
    let ctx = g.ctx_over(&store);
    let check = recording(&g.beside("seen"));
    swarmops_ws::land(&ctx, land_of("w", Some(&check))).unwrap_err();
    let dir = g.beside("w");
    assert_eq!(state::read(&RealFs::new(), &dir).unwrap().head, head);
    let done = swarmops_ws::land(&ctx, land_of("w", Some(&check))).unwrap();
    let rebases = store.rebases();
    assert_eq!(rebases.len(), 2, "{rebases:?}");
    assert_eq!(rebases[0].changeset, rebases[1].changeset);
    assert_eq!(rebases[1].status, RebaseStatus::Replayed);
    assert_eq!(store.lands().len(), 1);
    assert_eq!(show(&g, &done.land.landed).parents, [main]);
    assert_eq!(main_of(&g), done.land.landed);
}

/// Without a check, `ws land` is the store's land of the workspace's chain, merged onto main's
/// head wherever it moved, and the workspace then stands on what landed.
#[test]
fn ws_land_without_check_merges_as_land_does() {
    // 1. A workspace behind main.
    let f = Fixture::new();
    let (head, main) = behind(&f);

    // 2. Landed: one changeset on main's head, squashed from the workspace's commit.
    let done = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap();
    assert_eq!(done.attempts, 1);
    assert_eq!(done.land.status, LandStatus::Landed);
    assert_eq!(done.land.rebased_onto, main);
    assert_eq!(done.land.squashed_from, [head]);
    assert_eq!(main_of(&f), done.land.landed);

    // 3. The workspace stands on it, main's file written in, clean.
    let dir = f.beside("w");
    let saved = state::read(&RealFs::new(), &dir).unwrap();
    assert_eq!(
        (&saved.base, &saved.head),
        (&done.land.landed, &done.land.landed)
    );
    assert_eq!(saved.materialized, done.land.landed);
    assert_eq!(saved.onto, None);
    assert_eq!(
        tree_of(&dir),
        [file("src/a.txt", "mine\n"), file("src/b.txt", "b\n")]
    );
    assert_eq!(changes(&f.ctx(), "w", true), []);
    assert!(!pending_in(&dir).exists());
}

/// An exact land of a workspace not rebased since its last commit is refused before anything
/// is written; after a rebase it lands the rebased tree.
#[test]
fn exact_land_without_a_rebase_is_refused() {
    // 1. A workspace behind main, committed since it was made.
    let f = Fixture::new();
    let (_, main) = behind(&f);
    let mut exact = land_of("w", None);
    exact.mode = Some(LandMode::Exact);

    // 2. Refused, naming the rebase it needs; nothing landed.
    let store = Hooked::new(&f.ops);
    let refused = swarmops_ws::land(&f.ctx_over(&store), exact.clone()).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert_eq!(refused.details["missing"], "rebase");
    assert_eq!(store.lands().len(), 0);
    assert_eq!(main_of(&f), main);

    // 3. Rebased, then landed exactly: L's root is H''s.
    let mut rebase = WsRebaseReq::new("w".to_owned());
    rebase.onto = "main".to_owned();
    rebase.action = WsRebaseAction::Start;
    rebase.status_after = StatusAfter::None;
    let rebased = swarmops_ws::rebase(&f.ctx(), rebase).unwrap();
    let done = swarmops_ws::land(&f.ctx(), exact).unwrap();
    assert_eq!(done.land.rebased_onto, main);
    assert_eq!(
        show(&f, &done.land.landed).root,
        show(&f, &rebased.changeset).root
    );
}

/// `--check` with no command given and none in the repository's `.swarmops/workspaces.json` is
/// refused before anything is rebased or landed; with one there, that command runs.
#[cfg(unix)]
#[test]
fn a_check_land_without_a_check_command_is_refused() {
    // 1. A workspace behind main; a check without a command.
    let f = Fixture::new();
    let (head, main) = behind(&f);
    let mut req = land_of("w", None);
    let mut check = WsCheckInput::default();
    check.timeout_s = 900;
    req.check = Some(check);

    // 2. Refused, naming the check; nothing rebased or landed.
    let store = Hooked::new(&f.ops);
    let refused = swarmops_ws::land(&f.ctx_over(&store), req.clone()).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert_eq!(refused.details["missing"], "check");
    assert_eq!((store.rebases().len(), store.lands().len()), (0, 0));
    assert_eq!(
        state::read(&RealFs::new(), &f.beside("w")).unwrap().head,
        head
    );

    // 3. The repository names one: it runs, and the land goes through.
    let seen = f.beside("seen");
    let argv = serde_json::to_string(&recording(&seen)).unwrap();
    let config = format!("{{\"check\": {{\"argv\": {argv}}}}}");
    let main = f.commit(
        Some(&main),
        &[Put::File(".swarmops/workspaces.json", &config)],
    );
    let done = swarmops_ws::land(&f.ctx(), req).unwrap();
    assert_eq!(done.land.rebased_onto, main);
    assert_eq!(seen_lines(&seen).len(), 1);
}

/// The root entries of changeset `id`'s tree, by name with their ids, but `src`.
fn outside_src(f: &Fixture, id: &str) -> Vec<(String, String)> {
    let listing = f.ops.read_tree(ReadTreeReq::new(id)).unwrap();
    (listing.entries.into_iter())
        .filter(|entry| entry.name != "src")
        .map(|entry| (entry.name, entry.id))
        .collect()
}

/// A sparse workspace's land moves only its own chain: every path outside its prefixes is the
/// target's, though the target moved one of them since the workspace's base.
#[test]
fn a_sparse_land_keeps_every_outside_path() {
    // 1. A base with `docs` and `src`; a workspace of `src` on it, with an edit committed.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[
            Put::File("README", "readme\n"),
            Put::File("docs/a", "a\n"),
            Put::File("docs/b", "b\n"),
            Put::File("src/a", "a\n"),
            Put::File("src/b", "b\n"),
        ],
    );
    f.create("w", &base, &["src"]);
    write(&f.beside("w"), "src/b", "mine\n");
    let mut req = WsCommitReq::new("w".to_owned(), "mine".to_owned());
    req.status_after = StatusAfter::None;
    swarmops_ws::commit(&f.ctx(), req).unwrap();

    // 2. Main moves `docs/a`; the workspace lands.
    let main = f.commit(Some(&base), &[Put::File("docs/a", "moved\n")]);
    let done = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap();
    assert_eq!(done.land.rebased_onto, main);

    // 3. Outside `src` the landed tree is main's; inside, the workspace's edit.
    let landed = &done.land.landed;
    assert_eq!(outside_src(&f, landed), outside_src(&f, &main));
    let mut read = ReadFileReq::new(landed.as_str(), "src/b");
    read.max_bytes = 64;
    assert_eq!(f.ops.read_file(read).unwrap().content, "mine\n");
}

/// `ws land` of a sparse workspace with an edit outside its prefixes is
/// `E_WS_OUTSIDE_PREFIXES` before anything else: no edit committed, nothing rebased or landed,
/// no store call but the workspace's open and the reads of its tree.
#[test]
fn ws_land_refuses_an_outside_edit() {
    // 1. A workspace of `src` with an edit inside and a stray file outside.
    let f = Fixture::new();
    let base = f.commit(
        None,
        &[Put::File("docs/a", "a\n"), Put::File("src/a", "a\n")],
    );
    f.create("w", &base, &["src"]);
    let dir = f.beside("w");
    write(&dir, "src/a", "mine\n");
    write(&dir, "docs/a", "stray\n");
    let before = state::read(&RealFs::new(), &dir).unwrap();

    // 2. A land that would commit the edit: refused, naming `docs`.
    let watched = Watched::new(&f.ops);
    let mut req = land_of("w", None);
    req.message = Some("mine".to_owned());
    let refused = swarmops_ws::land(&f.ctx_over(&watched), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");
    assert_eq!(refused.details["paths"], serde_json::json!(["docs"]));

    // 3. Nothing changed: the store only opened the workspace and read its tree; its state and
    //    files as they were.
    let calls = watched.calls();
    assert!(
        calls.iter().all(|call| OPEN_AND_READ.contains(call)),
        "{calls:?}"
    );
    assert_eq!(state::read(&RealFs::new(), &dir).unwrap(), before);
    assert_eq!(main_of(&f), base);
    assert_eq!(
        tree_of(&dir),
        [file("docs/a", "stray\n"), file("src/a", "mine\n")]
    );
    assert!(!pending_in(&dir).exists());
}

/// A checked land with no command given and none configured.
fn unconfigured_check(name: &str) -> WsLandReq {
    let mut out = land_of(name, None);
    let mut check = WsCheckInput::default();
    check.timeout_s = 900;
    out.check = Some(check);
    out
}

/// The outside refusal comes first: before a checked land resolves its command and before the
/// lock, so a land with `--check`, no command and a stray outside the prefixes is
/// `E_WS_OUTSIDE_PREFIXES`, with no store call but the workspace's open and the reads of its tree.
#[test]
fn an_outside_edit_is_refused_before_the_check() {
    // 1. A workspace of `src` with a stray file outside.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a", "a\n")]);
    f.create("w", &base, &["src"]);
    write(&f.beside("w"), "docs/a", "stray\n");

    // 2. Refused for the stray, not for the missing command nor the lock another holds.
    let _held = lock::acquire(&f.beside("w")).unwrap();
    let watched = Watched::new(&f.ops);
    let refused = swarmops_ws::land(&f.ctx_over(&watched), unconfigured_check("w")).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");
    assert_eq!(refused.details["paths"], serde_json::json!(["docs"]));

    // 3. The store only opened the workspace and read its tree.
    let calls = watched.calls();
    assert!(
        calls.iter().all(|call| OPEN_AND_READ.contains(call)),
        "{calls:?}"
    );
}

/// The mode bits of a directory made unreadable, given back when dropped.
#[cfg(unix)]
struct Unreadable(camino::Utf8PathBuf);

#[cfg(unix)]
impl Unreadable {
    /// Directory `dir`, made unreadable.
    fn new(dir: camino::Utf8PathBuf) -> Unreadable {
        set_mode(&dir, 0o000);
        Unreadable(dir)
    }
}

#[cfg(unix)]
impl Drop for Unreadable {
    fn drop(&mut self) {
        set_mode(&self.0, 0o755);
    }
}

/// Sets the permission bits of `path` to `mode`.
#[cfg(unix)]
fn set_mode(path: &Utf8Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A directory the outside walk cannot read stops `ws land` with `E_IO`, before the check is
/// resolved: the walk fails closed. Skipped where an unreadable directory can still be read
/// (as root).
#[cfg(unix)]
#[test]
fn an_unreadable_walk_stops_ws_land() {
    // 1. A workspace of `src/a`; `src`, which the walk reads to reach it, made unreadable.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a/f", "f\n")]);
    f.create("w", &base, &["src/a"]);
    let src = f.beside("w").join("src");
    let held = Unreadable::new(src.clone());
    if std::fs::read_dir(&src).is_ok() {
        eprintln!("skipped: an unreadable directory can be read here (root)");
        return;
    }

    // 2. A checked land with no command: `E_IO` from the walk, not the check's `E_INVALID_ARG`.
    let watched = Watched::new(&f.ops);
    let refused = swarmops_ws::land(&f.ctx_over(&watched), unconfigured_check("w"));
    drop(held);
    let refused = refused.unwrap_err();
    assert_eq!(refused.code, "E_IO", "{refused:?}");
    assert_eq!(refused.details["file"], ".");

    // 3. The store only opened the workspace.
    let calls = watched.calls();
    assert!(calls.iter().all(|call| *call == "repo_info"), "{calls:?}");
}

/// A workspace of the whole tree, also one with neither prefixes nor `all` asked for, has
/// nothing off its prefixes: `ws land` does not walk it, so a directory it cannot read does not
/// stop the land before the check is resolved (the check's `E_INVALID_ARG` comes first).
/// Skipped where an unreadable directory can still be read (as root).
#[cfg(unix)]
#[test]
fn a_whole_tree_is_not_walked_before_land() {
    for how in BARE {
        // 1. A workspace with no prefixes; `docs` made unreadable.
        let f = Fixture::new();
        let base = f.commit(
            None,
            &[Put::File("docs/d.md", "d\n"), Put::File("src/a.txt", "a\n")],
        );
        let docs = f.create_bare("w", &base, how).join("docs");
        let held = Unreadable::new(docs.clone());
        if std::fs::read_dir(&docs).is_ok() {
            eprintln!("skipped: an unreadable directory can be read here (root)");
            return;
        }

        // 2. A checked land with no command: the check's refusal, not the walk's `E_IO`.
        let refused = swarmops_ws::land(&f.ctx(), unconfigured_check("w"));
        drop(held);
        let refused = refused.unwrap_err();
        assert_eq!(refused.code, "E_INVALID_ARG", "{how:?} {refused:?}");
    }
}

/// What a refused land must leave as it was: the workspace's files and state, the pending
/// record, and the store's journal.
#[derive(PartialEq)]
struct Snapshot {
    /// The workspace's files.
    files: Vec<(String, char, String)>,
    /// Its `workspace.json`.
    state: state::WorkspaceFile,
    /// The pending record's bytes, if there is one.
    pending: Option<Vec<u8>>,
    /// The newest operation's number.
    op: u64,
}

impl Snapshot {
    /// Workspace `name`'s files and state, and the journal's end.
    fn of(f: &Fixture, name: &str) -> Snapshot {
        let dir = f.beside(name);
        Snapshot {
            files: tree_of(&dir),
            state: state::read(&RealFs::new(), &dir).unwrap(),
            pending: std::fs::read(pending_in(&dir)).ok(),
            op: f.last_op(),
        }
    }
}

/// A workspace `w` on `release/r` at `b` (one commit above `p`, which `main` holds too), with a
/// commit of its own on `src/a.txt` and an unsettled edit of it. `p`, `b` and `w`'s head.
fn on_release(f: &Fixture) -> (String, String, String) {
    // 1. `p`, and `b` on it; `release/r` at `b` (not protected, so it may move back).
    let p = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    let b = f.side(Some(&p), &[Put::File("src/b.txt", "b\n")]);
    f.point("release/r", &b);

    // 2. A workspace of it, one commit and one edit not committed.
    f.create("w", "release/r", &[]);
    let dir = f.beside("w");
    write(&dir, "src/a.txt", "mine\n");
    let mut req = WsCommitReq::new("w".to_owned(), "mine".to_owned());
    req.status_after = StatusAfter::None;
    let head = swarmops_ws::commit(&f.ctx(), req).unwrap().changeset;
    write(&dir, "src/a.txt", "unsettled\n");
    (p, b, head)
}

/// Land of workspace `w` as `ws land -m` asks, which would commit an edit.
fn land_with_message() -> WsLandReq {
    let mut req = land_of("w", None);
    req.message = Some("settle".to_owned());
    req
}

/// A land of a workspace whose target moved back off its base is `E_WS_STALE`: nothing is
/// settled, landed or written, and `details` names the target, the base, the head and the two
/// operations the journal holds.
#[test]
fn a_land_after_a_move_back_is_stale() {
    // 1. The workspace on `b`; `release/r` moved back to `p`.
    let f = Fixture::new();
    let (p, b, _) = on_release(&f);
    f.point("release/r", &p);
    let (base_op, _) = f.op_moving("release/r", &b);
    let (moved_op, _) = f.op_moving("release/r", &p);
    let before = Snapshot::of(&f, "w");

    // 2. Refused, even where `-m` would commit the edit.
    let refused = swarmops_ws::land(&f.ctx(), land_with_message()).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(
        serde_json::Value::Object(refused.details.clone()),
        serde_json::json!({
            "target": "release/r",
            "base": b,
            "head": p,
            "base_op": base_op,
            "moved_op": moved_op,
        })
    );
    assert_eq!(refused.hint.as_deref(), Some("swarmops ws rebase"));

    // 3. Every file, the state and the journal as they were; no record.
    let after = Snapshot::of(&f, "w");
    assert!(after == before, "the refused land wrote something");
    assert!(!pending_in(&f.beside("w")).exists());
}

/// The base's operation is found past the first page of the journal.
#[test]
fn a_stale_land_reads_the_journal_past_one_page() {
    // 1. The base's operation, then more than a page of other moves, then the move back.
    let f = Fixture::new();
    let (p, b, _) = on_release(&f);
    for n in 0..120 {
        f.point("release/other", if n % 2 == 0 { &p } else { &b });
    }
    f.point("release/r", &p);
    let (base_op, _) = f.op_moving("release/r", &b);
    let (moved_op, _) = f.op_moving("release/r", &p);
    assert!(moved_op - base_op > 100, "{base_op} {moved_op}");

    // 2. Both numbers found.
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(refused.details["base_op"], serde_json::json!(base_op));
    assert_eq!(refused.details["moved_op"], serde_json::json!(moved_op));
}

/// A target rewritten to an unrelated changeset leaves the base off its history.
#[test]
fn a_rewritten_target_is_stale() {
    // 1. `release/r` moved to a changeset that shares nothing with the base.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    let other = f.side(None, &[Put::File("z.txt", "z\n")]);
    f.point("release/r", &other);
    let (base_op, _) = f.op_moving("release/r", &b);
    let (moved_op, _) = f.op_moving("release/r", &other);
    let before = Snapshot::of(&f, "w");

    // 2. Refused, nothing written.
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(refused.details["head"], serde_json::json!(other));
    assert_eq!(refused.details["base_op"], serde_json::json!(base_op));
    assert_eq!(refused.details["moved_op"], serde_json::json!(moved_op));
    let after = Snapshot::of(&f, "w");
    assert!(after == before, "the refused land wrote something");
    assert!(!pending_in(&f.beside("w")).exists());
}

/// An `op undo` of the move that put the target on the base takes it off again: stale.
#[test]
fn a_land_after_an_undo_is_stale() {
    // 1. `release/r` at `p`, moved forward to `b`; a workspace of `b`; that move undone.
    let f = Fixture::new();
    let p = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.point("release/r", &p);
    let b = f.side(Some(&p), &[Put::File("src/b.txt", "b\n")]);
    f.point("release/r", &b);
    let (base_op, id) = f.op_moving("release/r", &b);
    f.create("w", "release/r", &[]);
    let who = swarmops_api::Identity::parse(crate::fixture::AGENT).unwrap();
    f.ops.undo(&who, UndoReq::new(id)).unwrap();
    let (moved_op, _) = f.op_moving("release/r", &p);
    assert_ne!(moved_op, base_op);
    let before = Snapshot::of(&f, "w");

    // 2. Refused, nothing written.
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_eq!(refused.details["base_op"], serde_json::json!(base_op));
    assert_eq!(refused.details["moved_op"], serde_json::json!(moved_op));
    let after = Snapshot::of(&f, "w");
    assert!(after == before, "the refused land wrote something");
    assert!(!pending_in(&f.beside("w")).exists());
}

/// A base no operation put on the target (a changeset the workspace was made from, off the
/// target's history) is not stale: the land proceeds.
#[test]
fn a_workspace_from_a_side_changeset_is_not_stale() {
    // 1. A workspace on a side changeset, its target `release/r`, which has other work.
    let f = Fixture::new();
    let p = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    let side = f.side(Some(&p), &[Put::File("src/s.txt", "s\n")]);
    let other = f.side(Some(&p), &[Put::File("src/m.txt", "m\n")]);
    f.point("release/r", &p);
    f.point("release/r", &other);
    f.create("w", &side, &[]);
    let dir = f.beside("w");
    let mut file = state::read(&RealFs::new(), &dir).unwrap();
    file.target = "release/r".to_owned();
    state::replace(&RealFs::new(), &dir, &file).unwrap();
    write(&dir, "src/a.txt", "mine\n");

    // 2. Landed on `release/r`.
    let done = swarmops_ws::land(&f.ctx(), land_with_message()).unwrap();
    assert_eq!(done.land.status, LandStatus::Landed);
    let head = f.ops.resolve_rev(ResolveRevReq::new("release/r"));
    assert_eq!(head.unwrap().id, done.land.landed);
}

/// The outside refusal comes first, then the stale one, then the dirty one.
#[test]
fn a_stale_refusal_sits_between_the_outside_and_the_dirty_ones() {
    // 1. A sparse workspace of `src` on `b`, `release/r` moved back, an edit inside and a stray
    //    outside.
    let f = Fixture::new();
    let p = f.commit(
        None,
        &[Put::File("docs/d", "d\n"), Put::File("src/a", "a\n")],
    );
    let b = f.side(Some(&p), &[Put::File("src/b", "b\n")]);
    f.point("release/r", &b);
    f.create("w", "release/r", &["src"]);
    f.point("release/r", &p);
    let dir = f.beside("w");
    write(&dir, "src/a", "mine\n");
    write(&dir, "docs/d", "stray\n");

    // 2. The stray first.
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");

    // 3. Without it, stale, not dirty.
    std::fs::remove_dir_all(dir.join("docs")).unwrap();
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
}

/// A target that gained a name the workspace's volume cannot hold is refused before the store
/// lands anything: the target stays where it was, no store call writes, no pending record is
/// left and the workspace is as it was.
#[test]
fn a_land_onto_a_reserved_name_moves_nothing() {
    for reserved in ["src/.SwarmWs/z", ".swarmws/x"] {
        // 1. A workspace on `release/r` with an edit; `release/r` then gains `reserved`.
        let f = Fixture::new();
        let b = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
        f.point("release/r", &b);
        f.create("w", "release/r", &[]);
        let dir = f.beside("w");
        write(&dir, "src/a.txt", "mine\n");
        let gained = f.side(Some(&b), &[Put::File(reserved, "z\n")]);
        f.point("release/r", &gained);
        let before = Snapshot::of(&f, "w");
        let watched = Watched::new(&f.ops);

        // 2. The land is refused as unportable, and nothing was written.
        let refused = swarmops_ws::land(&f.ctx_over(&watched), land_with_message()).unwrap_err();
        assert_eq!(refused.code, "E_WS_UNPORTABLE", "{reserved}: {refused:?}");
        let head = f.ops.resolve_rev(ResolveRevReq::new("release/r"));
        assert_eq!(head.unwrap().id, gained, "{reserved}: the target moved");
        let writes = watched.count("land") + watched.count("commit");
        assert_eq!(writes, 0, "{reserved}: a store write");
        assert!(Snapshot::of(&f, "w") == before, "{reserved}: a write");
        assert!(!pending_in(&dir).exists(), "{reserved}: a pending record");
    }
}

/// The land of a workspace whose base is off the target (the target moved back) is not refused
/// as stale when it names another target: only the workspace's own target is judged.
#[test]
fn only_the_workspaces_own_target_is_judged_stale() {
    for other in ["main", "release/x"] {
        // 1. `release/r` moved back off the base; `other` is `main`, or a ref that held the base
        //    and moved off it too.
        let f = Fixture::new();
        let (p, b, _) = on_release(&f);
        if other != "main" {
            f.point(other, &b);
            f.point(other, &p);
        }
        f.point("release/r", &p);

        // 2. Landed on `other`: not refused as stale.
        let mut req = land_with_message();
        req.target = Some(other.to_owned());
        let done = swarmops_ws::land(&f.ctx(), req).unwrap_or_else(|e| panic!("{other}: {e:?}"));
        assert_eq!(done.land.status, LandStatus::Landed, "{other}");
    }
}

/// A history longer than the cap leaves the question open: not stale, and the journal is not
/// read.
#[test]
fn a_history_past_the_cap_is_not_stale() {
    // 1. A workspace of `main`'s old head; `main` ahead of it, its history reading 20 000 long.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a.txt", "a\n")]);
    f.create("w", &base, &[]);
    write(&f.beside("w"), "src/a.txt", "mine\n");
    f.commit(Some(&base), &[Put::File("src/b.txt", "b\n")]);
    let mut watched = Watched::new(&f.ops);
    watched.fake_history = Some(20_000);

    // 2. Landed, the journal unread.
    let done = swarmops_ws::land(&f.ctx_over(&watched), land_with_message()).unwrap();
    assert_eq!(done.land.status, LandStatus::Landed);
    assert_eq!(watched.count("op_log"), 0);
}

/// A sparse workspace is refused only for the names under its own prefixes: a name the target
/// gained elsewhere does not stop its land.
#[test]
fn a_reserved_name_outside_the_prefixes_lands() {
    // 1. A workspace of `src`; `release/r` gains a reserved name under `docs`.
    let f = Fixture::new();
    let p = f.commit(
        None,
        &[Put::File("docs/d", "d\n"), Put::File("src/a", "a\n")],
    );
    f.point("release/r", &p);
    f.create("w", "release/r", &["src"]);
    write(&f.beside("w"), "src/a", "mine\n");
    let gained = f.side(Some(&p), &[Put::File("docs/.swarmws/x", "z\n")]);
    f.point("release/r", &gained);

    // 2. Landed.
    let done = swarmops_ws::land(&f.ctx(), land_with_message()).unwrap();
    assert_eq!(done.land.status, LandStatus::Landed);
}

/// `moved_op` is the newest move of the target to its head, not the first.
#[test]
fn the_stale_refusal_names_the_newest_move_to_the_head() {
    // 1. `release/r` moved to `p`, away, and back to `p`.
    let f = Fixture::new();
    let (p, _, _) = on_release(&f);
    f.point("release/r", &p);
    let first = f.last_op();
    let away = f.side(Some(&p), &[Put::File("src/o.txt", "o\n")]);
    f.point("release/r", &away);
    f.point("release/r", &p);
    let newest = f.last_op();

    // 2. The refusal names the last of the two moves.
    let refused = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
    assert_ne!(refused.details["moved_op"], serde_json::json!(first));
    assert_eq!(refused.details["moved_op"], serde_json::json!(newest));
}

/// Cuts a `ws land -m` of workspace `w` short, before the store's land (`Cut::Before`) or after
/// its frame (`Cut::After`); the workspace's head then.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Cut {
    /// The store never sees the land.
    Before,
    /// The store records the land, then the call fails.
    After,
}

/// The cut land, and the workspace's head after it.
fn cut_land(f: &Fixture, store: &Hooked<'_>, cut: Cut) -> String {
    let flag = match cut {
        Cut::Before => &store.crash_before_land,
        Cut::After => &store.crash_after_land,
    };
    flag.store(true, Ordering::SeqCst);
    let cut = swarmops_ws::land(&f.ctx_over(store), land_with_message()).unwrap_err();
    assert_eq!(cut.code, "E_IO", "{cut:?}");
    assert!(pending_in(&f.beside("w")).exists());
    state::read(&RealFs::new(), &f.beside("w")).unwrap().head
}

/// `ws rebase -m` of workspace `w` onto `release/r`, which would commit an edit.
fn rebase_settling(ctx: &swarmops_ws::Ctx<'_>) -> Result<swarmops_api::WsRebaseResp, OpError> {
    let mut req = WsRebaseReq::new("w".to_owned());
    req.action = WsRebaseAction::Start;
    "release/r".clone_into(&mut req.onto);
    req.message = Some("settle".to_owned());
    req.status_after = StatusAfter::None;
    swarmops_ws::rebase(ctx, req)
}

/// `ws rebase` of workspace `w` onto `release/r`.
fn rebase_w(ctx: &swarmops_ws::Ctx<'_>) -> Result<swarmops_api::WsRebaseResp, OpError> {
    let mut req = WsRebaseReq::new("w".to_owned());
    req.action = WsRebaseAction::Start;
    "release/r".clone_into(&mut req.onto);
    req.status_after = StatusAfter::None;
    swarmops_ws::rebase(ctx, req)
}

/// A land cut short whose target then left the base is not a dead end: `ws land` refuses as
/// stale and writes nothing, `ws rebase` takes over the record and moves the workspace, and the
/// land after it goes through. Whether the store's frame was written does not matter: no land
/// of the record's key is on the target.
#[test]
fn a_cut_land_the_target_left_is_taken_over_by_a_rebase() {
    for cut in [Cut::After, Cut::Before] {
        // 1. A land cut short; the target moved back to `p`, off the base `b`.
        let f = Fixture::new();
        let (p, _, _) = on_release(&f);
        let store = Hooked::new(&f.ops);
        let head = cut_land(&f, &store, cut);
        f.point("release/r", &p);
        let ctx = f.ctx_over(&store);
        let dir = f.beside("w");
        let before = Snapshot::of(&f, "w");

        // 2. The land is refused as stale; the record and the rest as they were.
        let refused = swarmops_ws::land(&ctx, land_of("w", None)).unwrap_err();
        assert_eq!(refused.code, "E_WS_STALE", "{cut:?}: {refused:?}");
        assert!(Snapshot::of(&f, "w") == before, "{cut:?}: a write");
        assert_eq!(
            store.lands().len(),
            usize::from(cut == Cut::After),
            "{cut:?}"
        );

        // 3. The rebase takes the record over: the workspace on `p`, the record gone.
        let done = rebase_w(&ctx).unwrap_or_else(|e| panic!("{cut:?}: {e:?}"));
        assert_eq!(done.onto, p, "{cut:?}");
        let saved = state::read(&RealFs::new(), &dir).unwrap();
        assert_eq!((&saved.base, &saved.target), (&p, &"release/r".to_owned()));
        assert_ne!(saved.head, head, "{cut:?}: not rebased");
        assert!(!pending_in(&dir).exists(), "{cut:?}: a record");

        // 4. The next land goes through, onto `p`.
        let landed = swarmops_ws::land(&ctx, land_of("w", None)).unwrap();
        assert_eq!(landed.land.status, LandStatus::Landed, "{cut:?}");
        assert_eq!(
            show(&f, &landed.land.landed).parents,
            std::slice::from_ref(&p),
            "{cut:?}"
        );
    }
}

/// A land cut short after the store's frame, with the target still holding it, is the land's to
/// finish: `ws rebase` leaves the record, and the land replays the same changeset, the target
/// holding one land of it, wherever the target moved on after it.
#[test]
fn a_cut_land_the_target_holds_is_finished_not_taken_over() {
    for moved_on in [false, true] {
        // 1. The land recorded, then cut short; the target, maybe, moved on from it.
        let f = Fixture::new();
        let (_, b, _) = on_release(&f);
        let store = Hooked::new(&f.ops);
        cut_land(&f, &store, Cut::After);
        let first = store.lands()[0].landed.clone();
        if moved_on {
            let next = f.side(Some(&first), &[Put::File("src/n.txt", "n\n")]);
            f.point("release/r", &next);
        }
        let ctx = f.ctx_over(&store);
        let dir = f.beside("w");

        // 2. `ws rebase` is told to finish the land first; the record stays.
        let refused = rebase_w(&ctx).unwrap_err();
        assert_eq!(refused.code, "E_INVALID_ARG", "{moved_on}: {refused:?}");
        assert!(pending_in(&dir).exists(), "{moved_on}");

        // 3. The land replays: the same changeset, the workspace on it.
        let done = swarmops_ws::land(&ctx, land_of("w", None)).unwrap();
        assert_eq!(done.land.status, LandStatus::Replayed, "{moved_on}");
        assert_eq!(done.land.landed, first, "{moved_on}");
        let landed: Vec<_> = store.lands().iter().map(|l| l.landed.clone()).collect();
        assert_eq!(landed, [first.clone(), first.clone()], "{moved_on}");
        assert_eq!(show(&f, &first).parents, [b], "{moved_on}");
        assert_eq!(state::read(&RealFs::new(), &dir).unwrap().head, first);
        assert!(!pending_in(&dir).exists(), "{moved_on}");
    }
}

/// A land cut short after its frame, which the target holds, finishes though the target then
/// gained a name the volume cannot hold: the replay writes nothing to the store, and the files
/// move to the land itself.
#[test]
fn a_held_land_replays_past_a_reserved_name() {
    // 1. The land cut short; the target moved on from it to a changeset with a reserved name.
    let f = Fixture::new();
    on_release(&f);
    let store = Hooked::new(&f.ops);
    cut_land(&f, &store, Cut::After);
    let first = store.lands()[0].landed.clone();
    let gained = f.side(Some(&first), &[Put::File("src/.swarmws/x", "z\n")]);
    f.point("release/r", &gained);

    // 2. The re-run replays the land and the workspace stands on it.
    let ctx = f.ctx_over(&store);
    let done = swarmops_ws::land(&ctx, land_of("w", None)).unwrap();
    assert_eq!(done.land.status, LandStatus::Replayed);
    let dir = f.beside("w");
    assert_eq!(state::read(&RealFs::new(), &dir).unwrap().head, first);
    assert!(!pending_in(&dir).exists());
}

/// A land cut short whose land is not on the target, which then gained a reserved name above
/// the base: the land is refused as unportable and writes nothing; `ws rebase` gives the same
/// refusal before it takes the record over, so the record stays for the land, which goes
/// through once the target is fixed.
#[test]
fn a_cut_land_and_a_reserved_name_leave_no_dead_end() {
    // 1. The land cut short after its frame; the target set to a child of the base with a
    //    reserved name, which does not hold the land.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    let store = Hooked::new(&f.ops);
    cut_land(&f, &store, Cut::After);
    let gained = f.side(Some(&b), &[Put::File("src/.swarmws/x", "z\n")]);
    f.point("release/r", &gained);
    let ctx = f.ctx_over(&store);
    let dir = f.beside("w");
    let before = Snapshot::of(&f, "w");

    // 2. The land is refused as unportable, the record kept as it was.
    let refused = swarmops_ws::land(&ctx, land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert!(Snapshot::of(&f, "w") == before);

    // 3. The rebase is refused the same way, before it takes the record over.
    let refused = rebase_w(&ctx).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert!(Snapshot::of(&f, "w") == before);
    assert!(pending_in(&dir).exists(), "the record went");

    // 4. The target fixed: the land goes through.
    f.point("release/r", &b);
    let done = swarmops_ws::land(&ctx, land_of("w", None)).unwrap();
    assert_eq!(done.land.status, LandStatus::Landed);
}

/// A workspace `w` on `release/r` at `b` with a commit adding `src/Readme`, on a volume that
/// folds case; `release/r` then gains `src/README`. `b`, `w`'s head and the target's head.
fn collision(f: &Fixture) -> (String, String, String) {
    // 1. The workspace on a volume that folds case, with a commit adding `src/Readme`.
    let b = folding(f, &[Put::File("src/a.txt", "a\n")]);
    write(&f.beside("w"), "src/Readme", "mine\n");
    let head = commit_w(f, "mine");

    // 2. The target gains `src/README` beside it.
    let gained = f.side(Some(&b), &[Put::File("src/README", "theirs\n")]);
    f.point("release/r", &gained);
    (b, head, gained)
}

/// A case collision between the workspace's new path and the target's is refused before the
/// store lands: the target stays, no store write is made, and no record is left.
#[test]
fn a_land_onto_a_case_collision_moves_nothing() {
    // 1. `src/Readme` of the workspace, `src/README` of the target.
    let f = Fixture::new();
    let (_, _, gained) = collision(&f);
    let before = Snapshot::of(&f, "w");
    let watched = Watched::new(&f.ops);

    // 2. Refused as unportable, nothing written.
    let refused = swarmops_ws::land(&f.ctx_over(&watched), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(refused.details["reason"], "case_collision");
    assert_eq!(watched.count("land"), 0);
    assert_eq!(head_of(&f, "release/r"), gained);
    assert!(Snapshot::of(&f, "w") == before);
}

/// The same for `ws rebase`: refused before the store rebases, so no changeset is recorded and no
/// record is left.
#[test]
fn a_rebase_onto_a_case_collision_records_nothing() {
    // 1. `src/Readme` of the workspace, `src/README` of the target.
    let f = Fixture::new();
    collision(&f);
    let before = Snapshot::of(&f, "w");
    let watched = Watched::new(&f.ops);

    // 2. Refused as unportable: no store write, no changeset, no record.
    let refused = rebase_w(&f.ctx_over(&watched)).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(watched.count("rebase"), 0);
    assert!(Snapshot::of(&f, "w") == before);
}

/// The head of ref `name`.
fn head_of(f: &Fixture, name: &str) -> String {
    f.ops.resolve_rev(ResolveRevReq::new(name)).unwrap().id
}

/// The call a refusal after the store's rebase is made by.
#[derive(Clone, Copy, Debug)]
enum Via {
    /// `ws rebase`.
    Rebase,
    /// `ws land --check`, whose rebase is the first step.
    CheckedLand,
}

/// A target that gains a reserved name between the preflight and the store's rebase (a race the
/// preflight cannot see): the store has rebased, the files cannot follow, and the record stays.
/// A re-run is refused before the store while the target holds the name, so it adds no rebase.
fn keeps_the_record(via: Via) {
    // 1. A workspace one commit above `b`; the target a commit ahead, and about to gain a
    //    reserved name as the store is called.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    write(&f.beside("w"), "src/a.txt", "mine\n");
    let c = f.side(Some(&b), &[Put::File("src/c.txt", "c\n")]);
    f.point("release/r", &c);
    let store = Hooked::new(&f.ops);
    let fixture = &f;
    let racer = c.clone();
    *store.before_rebase.lock().unwrap() = Some(Box::new(move || {
        let d = fixture.side(Some(&racer), &[Put::File("src/.swarmws/x", "z\n")]);
        fixture.point("release/r", &d);
    }));
    let ctx = f.ctx_over(&store);
    let dir = f.beside("w");
    let call = || match via {
        Via::Rebase => rebase_w(&ctx).unwrap_err(),
        Via::CheckedLand => {
            swarmops_ws::land(&ctx, land_of("w", Some(&sh("true", &[])))).unwrap_err()
        }
    };

    // 2. The call is refused as unportable, after the store's rebase.
    let refused = call();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{via:?}: {refused:?}");
    assert_eq!(store.rebases().len(), 1, "{via:?}");

    // 3. The record stays; a re-run is refused before the store, and adds no rebase.
    assert!(pending_in(&dir).exists(), "{via:?}: the record went");
    let again = call();
    assert_eq!(again.code, "E_WS_UNPORTABLE", "{via:?}: {again:?}");
    assert_eq!(store.rebases().len(), 1, "{via:?}: a second rebase");
    assert!(pending_in(&dir).exists(), "{via:?}");
}

/// `ws rebase`: see [`keeps_the_record`].
#[test]
fn a_refusal_after_the_store_rebase_keeps_the_record() {
    keeps_the_record(Via::Rebase);
}

/// `ws land --check`: see [`keeps_the_record`].
#[cfg(unix)]
#[test]
fn a_refusal_after_the_checked_rebase_keeps_the_record() {
    keeps_the_record(Via::CheckedLand);
}

/// The operations the search for a stale base or a land's key reads, as the library caps it
/// (`status::OP_CAP`, not public).
const JOURNAL_CAP: u64 = 10_000;

/// A workspace `w` on `release/r` at a commit of `files`, on a volume that folds case. The
/// commit.
fn folding(f: &Fixture, files: &[Put<'_>]) -> String {
    let b = f.commit(None, files);
    f.point("release/r", &b);
    f.create("w", "release/r", &[]);
    let mut volume = Volume::new(false, true, true);
    volume.case_insensitive = true;
    f.simulate("w", volume);
    b
}

/// Commits every edit of workspace `w` with `message`; the new head.
fn commit_w(f: &Fixture, message: &str) -> String {
    let mut req = WsCommitReq::new("w".to_owned(), message.to_owned());
    req.status_after = StatusAfter::None;
    swarmops_ws::commit(&f.ctx(), req).unwrap().changeset
}

/// A cut land whose head's operation lies past the journal's operation cap is not known to be
/// absent: the record stays, and `ws rebase` says to finish the land first.
#[test]
fn a_cut_land_the_journal_cannot_place_is_left() {
    // 1. A land cut before its frame, the target moved back; the cap spent above the store's
    //    own operations.
    let f = Fixture::new();
    let (p, _, _) = on_release(&f);
    let hooked = Hooked::new(&f.ops);
    cut_land(&f, &hooked, Cut::Before);
    f.point("release/r", &p);
    let mut watched = Watched::new(&hooked);
    watched.fake_ops = Some(JOURNAL_CAP);

    // 2. The rebase does not take the record over.
    let refused = rebase_w(&f.ctx_over(&watched)).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert!(pending_in(&f.beside("w")).exists(), "the record went");
}

/// The same when the target's history is longer than its cap: the land's changeset is not found
/// within it, and not known to be missing.
#[test]
fn a_cut_land_the_history_cannot_place_is_left() {
    let f = Fixture::new();
    on_release(&f);
    let hooked = Hooked::new(&f.ops);
    cut_land(&f, &hooked, Cut::After);
    let mut watched = Watched::new(&hooked);
    watched.fake_history_of = Some(("release/r".to_owned(), HISTORY_CAP * 2));

    let refused = rebase_w(&f.ctx_over(&watched)).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert!(pending_in(&f.beside("w")).exists(), "the record went");
}

/// A cut land not known to be held keeps the reserved-name preflight: it is not replayed past
/// a name the target gained.
#[test]
fn an_unplaced_land_keeps_the_preflight() {
    // 1. A land cut after its frame; the target moved on to a reserved name; its history too long.
    let f = Fixture::new();
    on_release(&f);
    let hooked = Hooked::new(&f.ops);
    cut_land(&f, &hooked, Cut::After);
    let first = hooked.lands()[0].landed.clone();
    let gained = f.side(Some(&first), &[Put::File("src/.swarmws/x", "z\n")]);
    f.point("release/r", &gained);
    let mut watched = Watched::new(&hooked);
    watched.fake_history_of = Some(("release/r".to_owned(), HISTORY_CAP * 2));

    // 2. The land is refused as unportable.
    let refused = swarmops_ws::land(&f.ctx_over(&watched), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
}

/// A journal past the operation cap is not read to its end: a base it cannot be shown to have
/// left is not stale.
#[test]
fn a_journal_past_the_cap_is_not_stale() {
    let f = Fixture::new();
    let (p, _, _) = on_release(&f);
    f.point("release/r", &p);
    let mut watched = Watched::new(&f.ops);
    watched.fake_ops = Some(JOURNAL_CAP);

    let done = swarmops_ws::land(&f.ctx_over(&watched), land_with_message()).unwrap();
    assert_eq!(done.land.status, LandStatus::Landed);
}

/// A base move under the cap is found past many newer operations: the land is stale.
#[test]
fn a_journal_under_the_cap_is_read() {
    let f = Fixture::new();
    let (p, _, _) = on_release(&f);
    f.point("release/r", &p);
    let mut watched = Watched::new(&f.ops);
    watched.fake_ops = Some(JOURNAL_CAP / 2);

    let refused = swarmops_ws::land(&f.ctx_over(&watched), land_with_message()).unwrap_err();
    assert_eq!(refused.code, "E_WS_STALE", "{refused:?}");
}

/// Only the record's key counts as the cut land: another workspace's land on the target since
/// does not make it held.
#[test]
fn another_workspaces_land_is_not_the_cut_one() {
    // 1. A land cut before its frame; another workspace lands on the target.
    let f = Fixture::new();
    on_release(&f);
    let store = Hooked::new(&f.ops);
    cut_land(&f, &store, Cut::Before);
    f.create("v", "release/r", &[]);
    write(&f.beside("v"), "src/v.txt", "v\n");
    let mut req = land_of("v", None);
    req.message = Some("v".to_owned());
    let done = swarmops_ws::land(&f.ctx(), req).unwrap();
    assert_eq!(done.land.status, LandStatus::Landed);

    // 2. The rebase takes the cut land's record over.
    rebase_w(&f.ctx_over(&store)).unwrap();
    assert!(!pending_in(&f.beside("w")).exists());
}

/// A checked land of `w` with `-m` whose command succeeds, cut where `arm` says; asserts the cut.
#[cfg(unix)]
fn cut_checked(f: &Fixture, store: &Hooked<'_>, arm: &AtomicBool) {
    arm.store(true, Ordering::SeqCst);
    let mut req = land_of("w", Some(&sh("true", &[])));
    req.message = Some("settle".to_owned());
    let cut = swarmops_ws::land(&f.ctx_over(store), req).unwrap_err();
    assert_eq!(cut.code, "E_IO", "{cut:?}");
    assert!(pending_in(&f.beside("w")).exists());
}

/// `ws rebase` of workspace `w` onto `onto`, with no `-m`.
#[cfg(unix)]
fn rebase_onto(
    ctx: &swarmops_ws::Ctx<'_>,
    onto: &str,
) -> Result<swarmops_api::WsRebaseResp, OpError> {
    let mut req = WsRebaseReq::new("w".to_owned());
    req.action = WsRebaseAction::Start;
    onto.clone_into(&mut req.onto);
    req.status_after = StatusAfter::None;
    swarmops_ws::rebase(ctx, req)
}

/// The key a changeset was committed under.
#[cfg(unix)]
fn commit_key(f: &Fixture, id: &str) -> Option<String> {
    show(f, id).metadata.get("commit_key").cloned()
}

/// A checked land cut after the store's rebase, which `ws rebase` onto the same ref takes over,
/// replays that rebase: the same changeset, and no new frame.
#[cfg(unix)]
#[test]
fn a_takeover_replays_the_cut_rebase() {
    // 1. A checked land cut after the store's rebase onto `c`.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    let c = f.side(Some(&b), &[Put::File("src/c.txt", "c\n")]);
    f.point("release/r", &c);
    let store = Hooked::new(&f.ops);
    cut_checked(&f, &store, &store.crash_after_rebase);
    let first = store.rebases()[0].clone();
    let recorded = f.last_op();

    // 2. The rebase replays it.
    let done = rebase_w(&f.ctx_over(&store)).unwrap();
    assert_eq!(done.changeset, first.changeset);
    assert_eq!(store.rebases()[1].status, RebaseStatus::Replayed);
    assert_eq!(f.last_op(), recorded, "a new frame");
    assert!(!pending_in(&f.beside("w")).exists());
}

/// A land cut at `landing`, after its rebase, is rebased anew by the takeover onto the target's
/// head now, under a new key.
#[cfg(unix)]
#[test]
fn a_takeover_of_a_landing_cut_takes_a_new_key() {
    // 1. A checked land cut after the rebase, before the land; the target moved on.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    let c = f.side(Some(&b), &[Put::File("src/c.txt", "c\n")]);
    f.point("release/r", &c);
    let store = Hooked::new(&f.ops);
    cut_checked(&f, &store, &store.crash_before_land);
    let first = commit_key(&f, &store.rebases()[0].changeset);
    assert!(first.is_some());
    let d = f.side(Some(&c), &[Put::File("src/d.txt", "d\n")]);
    f.point("release/r", &d);

    // 2. The takeover rebases onto `d` under another key.
    let done = rebase_w(&f.ctx_over(&store)).unwrap();
    assert_eq!(done.onto, d);
    assert_eq!(
        store.rebases().last().unwrap().status,
        RebaseStatus::Rebased
    );
    assert_ne!(
        commit_key(&f, &done.changeset),
        first,
        "the cut rebase's key"
    );
}

/// A takeover `--onto` another ref than the cut land's rebased onto commits under a new key.
#[cfg(unix)]
#[test]
fn a_takeover_onto_another_ref_takes_a_new_key() {
    // 1. A checked land cut after the store's rebase onto `release/r`; another ref `release/o`.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    let c = f.side(Some(&b), &[Put::File("src/c.txt", "c\n")]);
    f.point("release/r", &c);
    let store = Hooked::new(&f.ops);
    cut_checked(&f, &store, &store.crash_after_rebase);
    let first = commit_key(&f, &store.rebases()[0].changeset);
    assert!(first.is_some());
    let o = f.side(Some(&b), &[Put::File("src/o.txt", "o\n")]);
    f.point("release/o", &o);

    // 2. The takeover rebases onto `release/o` under another key.
    let done = rebase_onto(&f.ctx_over(&store), "release/o").unwrap();
    assert_eq!(done.onto, o);
    assert_eq!(
        store.rebases().last().unwrap().status,
        RebaseStatus::Rebased
    );
    assert_ne!(
        commit_key(&f, &done.changeset),
        first,
        "the cut rebase's key"
    );
}

/// A land onto another ref than the workspace's, cut after its frame and held there, is not
/// taken over by `ws rebase`: the record names the ref it landed on.
#[cfg(unix)]
#[test]
fn a_takeover_reads_the_records_target() {
    // 1. A land onto `release/o`, cut after its frame; `release/o` holds it.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    f.point("release/o", &b);
    let store = Hooked::new(&f.ops);
    store.crash_after_land.store(true, Ordering::SeqCst);
    let mut req = land_with_message();
    req.target = Some("release/o".to_owned());
    let cut = swarmops_ws::land(&f.ctx_over(&store), req).unwrap_err();
    assert_eq!(cut.code, "E_IO", "{cut:?}");

    // 2. The rebase leaves the record: its land is held, though not by the workspace's target.
    let refused = rebase_w(&f.ctx_over(&store)).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert!(pending_in(&f.beside("w")).exists(), "the record taken over");
}

/// A checked land whose target gains a reserved name before the land retries is refused before
/// the retry's rebase and its record.
#[cfg(unix)]
#[test]
fn a_retry_checks_the_target() {
    // 1. A checked land; the target gains a reserved name as its land is about to run.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    let store = Hooked::new(&f.ops);
    let fixture = &f;
    let racer = b.clone();
    *store.before_land.lock().unwrap() = Some(Box::new(move || {
        let d = fixture.side(Some(&racer), &[Put::File("src/.swarmws/x", "z\n")]);
        fixture.point("release/r", &d);
    }));
    let mut req = land_of("w", Some(&sh("true", &[])));
    req.message = Some("settle".to_owned());

    // 2. The retry is refused before a second rebase, and leaves no record.
    let refused = swarmops_ws::land(&f.ctx_over(&store), req).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(store.rebases().len(), 1, "a second rebase");
    assert!(!pending_in(&f.beside("w")).exists(), "a record");
}

/// A target that gains a reserved name between the preflight and the store's land is landed on,
/// and the landed changeset holds the name for good: a re-run replays it and is refused again,
/// though the target dropped the name since, and `ws rebase` leaves the record.
#[test]
fn a_land_race_is_not_freed_by_the_target_dropping_the_name() {
    // 1. A land; the target gains `src/.swarmws/x` as the store is called.
    let f = Fixture::new();
    let (_, b, _) = on_release(&f);
    let c = f.side(Some(&b), &[Put::File("src/c.txt", "c\n")]);
    f.point("release/r", &c);
    let store = Hooked::new(&f.ops);
    let fixture = &f;
    let racer = c.clone();
    *store.before_land.lock().unwrap() = Some(Box::new(move || {
        let d = fixture.side(Some(&racer), &[Put::File("src/.swarmws/x", "z\n")]);
        fixture.point("release/r", &d);
    }));
    let ctx = f.ctx_over(&store);
    let dir = f.beside("w");
    let first = swarmops_ws::land(&ctx, land_with_message()).unwrap_err();
    assert_eq!(first.code, "E_WS_UNPORTABLE", "{first:?}");
    assert!(pending_in(&dir).exists());

    // 2. The target drops the name: the re-run replays the land and is refused again.
    let head = head_of(&f, "release/r");
    f.point("main", &head);
    let dropped = f.commit(Some(&head), &[Put::Gone("src/.swarmws/x")]);
    f.point("release/r", &dropped);
    let again = swarmops_ws::land(&ctx, land_of("w", None)).unwrap_err();
    assert_eq!(again.code, "E_WS_UNPORTABLE", "{again:?}");
    let statuses: Vec<_> = store.lands().iter().map(|l| l.status).collect();
    assert_eq!(statuses, [LandStatus::Landed, LandStatus::Replayed]);

    // 3. The rebase leaves the record: the land is the one to finish.
    let refused = rebase_w(&ctx).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert!(pending_in(&dir).exists(), "the record went");
}

/// A path the workspace deleted is not its new one: a target that renamed it by case lands on a
/// volume that folds case.
#[test]
fn a_deleted_path_is_not_a_new_one() {
    // 1. The workspace deletes `src/Readme`; the target renames it to `src/README`.
    let f = Fixture::new();
    let b = folding(
        &f,
        &[
            Put::File("src/a.txt", "a\n"),
            Put::File("src/Readme", "r\n"),
        ],
    );
    std::fs::remove_file(f.beside("w").join("src/Readme")).unwrap();
    write(&f.beside("w"), "src/a.txt", "mine\n");
    commit_w(&f, "gone");
    let renamed = f.commit(
        Some(&b),
        &[Put::Gone("src/Readme"), Put::File("src/README", "R\n")],
    );
    f.point("release/r", &renamed);

    // 2. The land goes through.
    let done = swarmops_ws::land(&f.ctx(), land_of("w", None)).unwrap();
    assert_eq!(done.land.status, LandStatus::Landed);
}

/// The chain's new paths are read past the first page of the diff: a collision on the second
/// page is refused before the store lands.
#[test]
fn a_chain_is_read_past_one_page() {
    // 1. A chain of 1,000 new files and `src/z/Readme`; the target gains `src/z/README`.
    let f = Fixture::new();
    let b = folding(&f, &[Put::File("src/a.txt", "a\n")]);
    let dir = f.beside("w");
    for i in 0..1000 {
        write(&dir, &format!("src/a/f{i:04}"), "x\n");
    }
    write(&dir, "src/z/Readme", "mine\n");
    commit_w(&f, "many");
    let gained = f.side(Some(&b), &[Put::File("src/z/README", "theirs\n")]);
    f.point("release/r", &gained);
    let watched = Watched::new(&f.ops);

    // 2. Refused as a case collision, before the store lands.
    let refused = swarmops_ws::land(&f.ctx_over(&watched), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(watched.count("land"), 0);
}

/// A rename that only changes case, inside the chain, leaves no `src/readme` behind for
/// `src/README` to meet: it lands and rebases on a volume that folds case, the target moved on
/// or not.
#[test]
fn a_case_only_rename_is_not_a_collision() {
    for moved in [false, true] {
        for rebase in [false, true] {
            // 1. The workspace renames `src/readme` to `src/README`; the target, maybe, moves.
            let f = Fixture::new();
            let b = folding(
                &f,
                &[
                    Put::File("src/a.txt", "a\n"),
                    Put::File("src/readme", "r\n"),
                ],
            );
            let dir = f.beside("w");
            std::fs::remove_file(dir.join("src/readme")).unwrap();
            write(&dir, "src/README", "r\n");
            commit_w(&f, "rename");
            if moved {
                let ahead = f.side(Some(&b), &[Put::File("src/c.txt", "c\n")]);
                f.point("release/r", &ahead);
            }

            // 2. The call goes through.
            let ctx = f.ctx();
            let case = format!("moved={moved} rebase={rebase}");
            if rebase {
                rebase_w(&ctx).unwrap_or_else(|e| panic!("{case}: {e:?}"));
                continue;
            }
            let done = swarmops_ws::land(&ctx, land_of("w", None))
                .unwrap_or_else(|e| panic!("{case}: {e:?}"));
            assert_eq!(done.land.status, LandStatus::Landed, "{case}");
        }
    }
}

/// A rename by case beside a target path of the same name still collides: only the paths the
/// chain deletes leave the case set.
#[test]
fn a_renamed_path_still_meets_the_targets() {
    // 1. The workspace renames `src/readme` to `src/Readme`; the target gains `src/README`.
    let f = Fixture::new();
    let b = folding(
        &f,
        &[
            Put::File("src/a.txt", "a\n"),
            Put::File("src/readme", "r\n"),
        ],
    );
    let dir = f.beside("w");
    std::fs::remove_file(dir.join("src/readme")).unwrap();
    write(&dir, "src/Readme", "r\n");
    commit_w(&f, "rename");
    let gained = f.side(Some(&b), &[Put::File("src/README", "theirs\n")]);
    f.point("release/r", &gained);
    let watched = Watched::new(&f.ops);

    // 2. Refused as a case collision, before any store write.
    let refused = swarmops_ws::land(&f.ctx_over(&watched), land_of("w", None)).unwrap_err();
    assert_eq!(refused.code, "E_WS_UNPORTABLE", "{refused:?}");
    assert_eq!(refused.details["reason"], "case_collision");
    assert_eq!(watched.count("land"), 0);
}

/// Renames `src/Lib/a.rs` of workspace `w` to `src/lib/a.rs`; the base also holds
/// `src/Lib/keep.rs`, which stays, when `keep`. The base.
fn rename_directory(f: &Fixture, keep: bool) -> String {
    let mut files = vec![
        Put::File("src/a.txt", "a\n"),
        Put::File("src/Lib/a.rs", "x\n"),
    ];
    if keep {
        files.push(Put::File("src/Lib/keep.rs", "k\n"));
    }
    let b = folding(f, &files);
    let dir = f.beside("w");
    std::fs::remove_file(dir.join("src/Lib/a.rs")).unwrap();
    if !keep {
        std::fs::remove_dir(dir.join("src/Lib")).unwrap();
    }
    write(&dir, "src/lib/a.rs", "x\n");
    commit_w(f, "rename directory");
    b
}

/// A directory renamed by case inside the chain leaves no `src/Lib` for `src/lib` to meet: it
/// lands and rebases on a volume that folds case, the target moved on or not.
#[test]
fn a_case_only_rename_of_a_directory_is_not_a_collision() {
    for moved in [false, true] {
        for rebase in [false, true] {
            // 1. The workspace renames the directory; the target, maybe, moves.
            let f = Fixture::new();
            let b = rename_directory(&f, false);
            if moved {
                let ahead = f.side(Some(&b), &[Put::File("src/c.txt", "c\n")]);
                f.point("release/r", &ahead);
            }

            // 2. The call goes through.
            let ctx = f.ctx();
            let case = format!("moved={moved} rebase={rebase}");
            if rebase {
                rebase_w(&ctx).unwrap_or_else(|e| panic!("{case}: {e:?}"));
                continue;
            }
            let done = swarmops_ws::land(&ctx, land_of("w", None))
                .unwrap_or_else(|e| panic!("{case}: {e:?}"));
            assert_eq!(done.land.status, LandStatus::Landed, "{case}");
        }
    }
}

/// A file under the old spelling of the directory, the target's or the base's that the chain
/// leaves, keeps that directory: the renamed one collides with it.
#[test]
fn a_file_under_a_renamed_directory_still_collides() {
    for kept in [false, true] {
        for rebase in [false, true] {
            // 1. The workspace renames `a.rs` into `src/lib`; `src/Lib` stays by a file of the
            //    base or one the target adds.
            let f = Fixture::new();
            let b = rename_directory(&f, kept);
            if !kept {
                let gained = f.side(Some(&b), &[Put::File("src/Lib/b.rs", "b\n")]);
                f.point("release/r", &gained);
            }
            let watched = Watched::new(&f.ops);
            let ctx = f.ctx_over(&watched);

            // 2. Refused as a case collision, before any store write.
            let refused = if rebase {
                rebase_w(&ctx).unwrap_err()
            } else {
                swarmops_ws::land(&ctx, land_of("w", None)).unwrap_err()
            };
            let case = format!("kept={kept} rebase={rebase}");
            assert_eq!(refused.code, "E_WS_UNPORTABLE", "{case}: {refused:?}");
            assert_eq!(refused.details["reason"], "case_collision", "{case}");
            assert_eq!(watched.count("land") + watched.count("rebase"), 0, "{case}");
        }
    }
}

/// A path the chain adds and the edits `-m` commits then rename by case is one new path, the
/// edit's: the target moved on or not, the call goes through.
#[test]
fn an_edit_that_renames_a_chain_add_is_not_a_collision() {
    for moved in [false, true] {
        for rebase in [false, true] {
            // 1. The chain adds `src/readme`; the files rename it to `src/README`, uncommitted.
            let f = Fixture::new();
            let b = folding(&f, &[Put::File("src/a.txt", "a\n")]);
            let dir = f.beside("w");
            write(&dir, "src/readme", "r\n");
            commit_w(&f, "add");
            std::fs::remove_file(dir.join("src/readme")).unwrap();
            write(&dir, "src/README", "r\n");
            if moved {
                let ahead = f.side(Some(&b), &[Put::File("src/c.txt", "c\n")]);
                f.point("release/r", &ahead);
            }

            // 2. The call commits the rename and goes through.
            let ctx = f.ctx();
            let case = format!("moved={moved} rebase={rebase}");
            if rebase {
                rebase_settling(&ctx).unwrap_or_else(|e| panic!("{case}: {e:?}"));
                continue;
            }
            let done = swarmops_ws::land(&ctx, land_with_message())
                .unwrap_or_else(|e| panic!("{case}: {e:?}"));
            assert_eq!(done.land.status, LandStatus::Landed, "{case}");
        }
    }
}

/// A new path of the chain that the target's meets by case, deleted by the edits `-m` commits,
/// leaves nothing to collide: the call goes through.
#[test]
fn an_edit_that_deletes_a_colliding_add_goes_through() {
    for rebase in [false, true] {
        // 1. The chain adds `src/X`, the target `src/x`; the files delete `src/X`, uncommitted.
        let f = Fixture::new();
        let b = folding(&f, &[Put::File("src/a.txt", "a\n")]);
        let dir = f.beside("w");
        write(&dir, "src/X", "mine\n");
        commit_w(&f, "add");
        std::fs::remove_file(dir.join("src/X")).unwrap();
        write(&dir, "src/a.txt", "edited\n");
        let gained = f.side(Some(&b), &[Put::File("src/x", "theirs\n")]);
        f.point("release/r", &gained);

        // 2. The call goes through.
        let ctx = f.ctx();
        if rebase {
            rebase_settling(&ctx).unwrap_or_else(|e| panic!("rebase: {e:?}"));
            continue;
        }
        let done =
            swarmops_ws::land(&ctx, land_with_message()).unwrap_or_else(|e| panic!("land: {e:?}"));
        assert_eq!(done.land.status, LandStatus::Landed);
    }
}

/// The edits `-m` is about to commit are judged with the chain's before the store is called: a
/// new path of them that meets the target's by case refuses the call, and nothing is written.
#[test]
fn an_unsettled_edit_is_judged_before_the_store() {
    for rebase in [false, true] {
        // 1. An unsettled `src/Readme`; the target gains `src/README`.
        let f = Fixture::new();
        let b = folding(&f, &[Put::File("src/a.txt", "a\n")]);
        write(&f.beside("w"), "src/Readme", "mine\n");
        let gained = f.side(Some(&b), &[Put::File("src/README", "theirs\n")]);
        f.point("release/r", &gained);
        let before = Snapshot::of(&f, "w");
        let watched = Watched::new(&f.ops);
        let ctx = f.ctx_over(&watched);

        // 2. Refused as a case collision: the edit not committed, the store not called.
        let refused = if rebase {
            rebase_settling(&ctx).unwrap_err()
        } else {
            swarmops_ws::land(&ctx, land_with_message()).unwrap_err()
        };
        assert_eq!(refused.code, "E_WS_UNPORTABLE", "{rebase}: {refused:?}");
        assert_eq!(refused.details["reason"], "case_collision", "{rebase}");
        assert_eq!(
            watched.count("land") + watched.count("rebase"),
            0,
            "{rebase}"
        );
        assert_eq!(watched.count("commit"), 0, "{rebase}");
        assert!(Snapshot::of(&f, "w") == before, "{rebase}: a write");
    }
}

/// A key that a newer changeset of the journal also carries is held when an older one is on the
/// target: a land cut after its frame, the target moved back, the resume onto another ref cut
/// after its frame (the record still names the first target), the first target restored to the
/// first land. `ws rebase` leaves the record, and the land replays the first changeset.
#[test]
fn a_key_on_an_older_changeset_is_held() {
    // 1. The land on `release/r` cut; `release/r` back; the resume onto `release/x` cut too.
    let f = Fixture::new();
    let (p, _, _) = on_release(&f);
    let store = Hooked::new(&f.ops);
    cut_land(&f, &store, Cut::After);
    let first = store.lands()[0].landed.clone();
    f.point("release/r", &p);
    let other = f.side(Some(&p), &[Put::File("src/x.txt", "x\n")]);
    f.point("release/x", &other);
    store.crash_after_land.store(true, Ordering::SeqCst);
    let mut req = land_of("w", None);
    req.target = Some("release/x".to_owned());
    let cut = swarmops_ws::land(&f.ctx_over(&store), req).unwrap_err();
    assert_eq!(cut.code, "E_IO", "{cut:?}");
    let second = store.lands()[1].landed.clone();
    assert_ne!(first, second);

    // 2. `release/r` restored to the first land: `ws rebase` leaves the record.
    f.point("release/r", &first);
    let ctx = f.ctx_over(&store);
    let dir = f.beside("w");
    let refused = rebase_w(&ctx).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert!(pending_in(&dir).exists(), "the record went");

    // 3. The land replays the first changeset.
    let done = swarmops_ws::land(&ctx, land_of("w", None)).unwrap();
    assert_eq!(done.land.status, LandStatus::Replayed);
    assert_eq!(done.land.landed, first);
}

/// A cut land is placed back to the operation that made the record's head, however long the
/// journal runs on below it: the land was never recorded, or the target moved back past it, and
/// `ws rebase` takes the record over.
#[test]
fn a_cut_land_is_placed_in_a_long_journal() {
    for cut in [Cut::Before, Cut::After] {
        // 1. A land cut short, the target moved back; 10,050 older operations below the head's.
        let f = Fixture::new();
        let (p, _, _) = on_release(&f);
        let hooked = Hooked::new(&f.ops);
        cut_land(&f, &hooked, cut);
        f.point("release/r", &p);
        let mut watched = Watched::new(&hooked);
        watched.fake_older_ops = Some(JOURNAL_CAP + 50);

        // 2. The rebase takes the record over.
        rebase_w(&f.ctx_over(&watched)).unwrap_or_else(|e| panic!("{cut:?}: {e:?}"));
        assert!(!pending_in(&f.beside("w")).exists(), "{cut:?}: a record");
    }
}

/// A land the target holds stays the land's to finish in a long journal: `ws rebase` leaves the
/// record, and `ws land` replays it.
#[test]
fn a_held_cut_land_is_finished_in_a_long_journal() {
    // 1. A land cut after its frame, which the target holds; 10,050 older operations below.
    let f = Fixture::new();
    on_release(&f);
    let hooked = Hooked::new(&f.ops);
    cut_land(&f, &hooked, Cut::After);
    let first = hooked.lands()[0].landed.clone();
    let mut watched = Watched::new(&hooked);
    watched.fake_older_ops = Some(JOURNAL_CAP + 50);
    let ctx = f.ctx_over(&watched);

    // 2. The rebase leaves the record; the land replays.
    let refused = rebase_w(&ctx).unwrap_err();
    assert_eq!(refused.code, "E_INVALID_ARG", "{refused:?}");
    assert!(pending_in(&f.beside("w")).exists(), "the record went");
    let done = swarmops_ws::land(&ctx, land_of("w", None)).unwrap();
    assert_eq!(done.land.status, LandStatus::Replayed);
    assert_eq!(done.land.landed, first);
}

/// The search for a cut land stops at the operation that made the record's head, if that is
/// the last of the journal's cap: one operation more above it, the land is not known to be
/// missing, and the record stays.
#[test]
fn a_cut_land_is_placed_up_to_the_cap() {
    for pad in [0, 1] {
        // 1. A land cut after its frame, the target moved back; the head's operation, with
        //    the ones above it, `pad` short of the cap or past it.
        let f = Fixture::new();
        let (p, _, _) = on_release(&f);
        let hooked = Hooked::new(&f.ops);
        let head = cut_land(&f, &hooked, Cut::After);
        let land = hooked.lands()[0].landed.clone();
        f.point("release/r", &p);
        let newest = f.last_op();
        let depth = newest - f.op_recording(&head) + 1;
        assert!(
            newest - f.op_recording(&land) < depth,
            "the land is above the head"
        );
        let mut watched = Watched::new(&hooked);
        watched.fake_ops = Some(JOURNAL_CAP - depth + pad);

        // 2. Within the cap the rebase takes the record over; past it, it stays.
        let taken = rebase_w(&f.ctx_over(&watched));
        let kept = pending_in(&f.beside("w")).exists();
        if pad == 0 {
            assert!(taken.is_ok() && !kept, "{taken:?} kept={kept}");
            continue;
        }
        assert_eq!(taken.unwrap_err().code, "E_INVALID_ARG");
        assert!(kept, "the record went");
    }
}
