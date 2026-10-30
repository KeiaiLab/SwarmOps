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
use swarmops_ws::RealFs;
use swarmops_ws::state::{self, SWARMWS_DIR};

use crate::fixture::{Fixture, Put, Watched, changes, file, tree_of, write};

/// A store that forwards every call to `inner`, and can run a hook before the first `land` (a
/// second process landing first) or fail right after a `land` or a `rebase` the store recorded,
/// as a process killed there would.
struct Hooked<'a> {
    /// The store the calls go to.
    inner: &'a dyn StoreOps,
    /// Run once, before the first `land` reaches the store.
    before_land: Mutex<Option<Box<dyn FnOnce() + Send + 'a>>>,
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

        // 2. The land, recorded; a crash after it, if asked for.
        let done = self.inner.land(who, req)?;
        self.lands.lock().unwrap().push(done.clone());
        if self.crash_after_land.swap(false, Ordering::SeqCst) {
            return Err(killed());
        }
        Ok(done)
    }

    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError> {
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
/// replays the same `H'` and the same land, and main holds one land.
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
/// no store call but the workspace's open.
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

    // 3. Nothing changed: the store only opened the workspace; its state and files as they were.
    let calls = watched.calls();
    assert!(calls.iter().all(|call| *call == "repo_info"), "{calls:?}");
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
/// `E_WS_OUTSIDE_PREFIXES`, with no store call but the workspace's open.
#[test]
fn an_outside_edit_is_refused_before_the_check() {
    // 1. A workspace of `src` with a stray file outside.
    let f = Fixture::new();
    let base = f.commit(None, &[Put::File("src/a", "a\n")]);
    f.create("w", &base, &["src"]);
    write(&f.beside("w"), "docs/a", "stray\n");

    // 2. Refused for the stray, not for the missing command.
    let watched = Watched::new(&f.ops);
    let refused = swarmops_ws::land(&f.ctx_over(&watched), unconfigured_check("w")).unwrap_err();
    assert_eq!(refused.code, "E_WS_OUTSIDE_PREFIXES", "{refused:?}");
    assert_eq!(refused.details["paths"], serde_json::json!(["docs"]));

    // 3. The store only opened the workspace.
    let calls = watched.calls();
    assert!(calls.iter().all(|call| *call == "repo_info"), "{calls:?}");
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
