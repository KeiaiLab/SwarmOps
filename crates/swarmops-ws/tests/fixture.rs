//! The fixture the workspace tests share: a store made through `LocalOps::init` in a temporary
//! directory and a workspace made through `create`, over the real file layers or a double (first
//! written by the M5 plan's task W5, then W6's edit helpers and W10's sweep driver).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{
    BlobGetReq, BlobGetResp, BlobHasReq, BlobHasResp, BlobPutReq, BlobPutResp, ChangeInput,
    ChangeOp, ChangesetDetail, ChangesetSummary, CommitReq, CommitResp, ContentInput, DeriveReq,
    DeriveResp, Diff, DiffReq, ErrorCode, FileContent, Identity, InfoReq, LandReq, LandResp,
    LogPage, LogReq, ModeInput, OpError, OpLogPage, OpLogReq, PathHistoryPage, PathHistoryReq,
    ReadFileReq, ReadTreeReq, RebaseReq, RebaseResp, RefDeleteReq, RefListReq, RefListResp,
    RefSetReq, RefSetResp, RepoInfo, ResolveRevReq, ResolvedRev, ShowReq, StatusAfter, StoreOps,
    TreeListing, UndoReq, UndoResp, VerifyReport, VerifyReq, WsChangeKind, WsCreateReq, WsInfo,
    WsStatusReq,
};
use swarmops_ops::LocalOps;
use swarmops_store::{FsOps, RealFs};
use swarmops_ws::state::{self, Volume};
use swarmops_ws::workspace::{Locate, Workspace};
use swarmops_ws::{Ctx, HostFs, RealHost};

/// The identity the fixture's mutations act as.
pub const AGENT: &str = "agent/tester";

/// One entry of a commit the fixture makes: a path and what it holds, or its removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put<'a> {
    /// A regular file holding the text.
    File(&'a str, &'a str),
    /// An executable file holding the text.
    Exec(&'a str, &'a str),
    /// A symlink holding the target.
    Link(&'a str, &'a str),
    /// The path removed.
    Gone(&'a str),
}

/// A temporary directory and a store made in it, at `<tmp>/store`; workspaces go beside it.
pub struct Fixture {
    _tmp: tempfile::TempDir,
    /// The store's root.
    pub root: Utf8PathBuf,
    /// The store's operations.
    pub ops: LocalOps,
}

impl Fixture {
    /// A fresh store named `acme`, without a changeset.
    pub fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().join("store");
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        Fixture {
            _tmp: tmp,
            root,
            ops,
        }
    }

    /// What the workspace operations run with, over the host `host`, as the fixture's agent.
    pub fn ctx_on(&self, host: Arc<dyn HostFs>) -> Ctx<'_> {
        self.ctx_with(host, Arc::new(RealFs::new()))
    }

    /// What the workspace operations run with, over the host `host` and the durable files' `fs`.
    pub fn ctx_with(&self, host: Arc<dyn HostFs>, fs: Arc<dyn FsOps>) -> Ctx<'_> {
        Ctx::new(
            &self.ops,
            self.root.clone(),
            Some(Identity::parse(AGENT).unwrap()),
            fs,
            host,
        )
    }

    /// What the workspace operations run with, over the real host, with `store` in place of the
    /// fixture's own (a [`Watched`] one over it).
    pub fn ctx_over<'s>(&'s self, store: &'s dyn StoreOps) -> Ctx<'s> {
        let mut ctx = self.ctx();
        ctx.store = store;
        ctx
    }

    /// What the workspace operations run with, over the real host.
    pub fn ctx(&self) -> Ctx<'_> {
        self.ctx_on(Arc::new(RealHost::default()))
    }

    /// A directory beside the store, not made.
    pub fn beside(&self, name: &str) -> Utf8PathBuf {
        self.root.parent().unwrap().join(name)
    }

    /// Commits `puts` on `parent` (a root commit when none) and moves `main` to it.
    pub fn commit(&self, parent: Option<&str>, puts: &[Put<'_>]) -> String {
        // 1. The change list: a put of inline content in its mode, or a delete.
        let changes = (puts.iter())
            .map(|put| {
                let (path, text, mode) = match *put {
                    Put::File(path, text) => (path, text, ModeInput::File),
                    Put::Exec(path, text) => (path, text, ModeInput::Exec),
                    Put::Link(path, target) => (path, target, ModeInput::Symlink),
                    Put::Gone(path) => {
                        return ChangeInput::new(path.to_owned(), ChangeOp::Delete);
                    }
                };
                let mut out = ChangeInput::new(path.to_owned(), ChangeOp::Put);
                out.content = Some(ContentInput::Inline(text.to_owned()));
                out.mode = Some(mode);
                out
            })
            .collect();

        // 2. Recorded on the parent, `main` moved along.
        let mut req = CommitReq::new(changes, "fixture".to_owned());
        req.parent = parent.map(Into::into);
        req.advance = Some("main".to_owned());
        let who = Identity::parse(AGENT).unwrap();
        self.ops.commit(&who, req).unwrap().changeset
    }

    /// Makes workspace `name` beside the store, on `base`, holding `prefixes` (the whole tree
    /// when empty), through `ctx`.
    pub fn create_in(
        &self,
        ctx: &Ctx<'_>,
        name: &str,
        base: &str,
        prefixes: &[&str],
    ) -> Result<WsInfo, OpError> {
        let mut req = WsCreateReq::new(name.to_owned());
        req.path = Some(self.beside(name).to_string());
        req.base = base.into();
        req.prefixes = prefixes.iter().map(|p| (*p).to_owned()).collect();
        req.all = prefixes.is_empty();
        req.status_after = StatusAfter::None;
        swarmops_ws::create(ctx, req)
    }

    /// Makes workspace `name` beside the store over the real host, as [`Fixture::create_in`].
    pub fn create(&self, name: &str, base: &str, prefixes: &[&str]) -> WsInfo {
        self.create_in(&self.ctx(), name, base, prefixes).unwrap()
    }

    /// Workspace `name`, opened.
    pub fn open(ctx: &Ctx<'_>, name: &str) -> Workspace {
        Workspace::open(ctx, Locate::Name(name.to_owned())).unwrap()
    }

    /// Records `volume` in workspace `name`'s state, as a volume that holds it would have been
    /// probed: how the tests simulate a volume this host has none of.
    pub fn simulate(&self, name: &str, volume: Volume) {
        let dir = self.beside(name);
        let mut file = state::read(&RealFs::new(), &dir).unwrap();
        file.volume = volume;
        state::replace(&RealFs::new(), &dir, &file).unwrap();
    }
}

/// The files below `dir`, by repository path, with what each is (`f` a file, `x` an executable,
/// `l` a symlink) and holds: `.swarmws/` left out.
pub fn tree_of(dir: &Utf8Path) -> Vec<(String, char, String)> {
    let mut found = Vec::new();
    walk(dir, dir, &mut found);
    found.sort();
    found
}

/// [`tree_of`]'s walk below `at`.
fn walk(root: &Utf8Path, at: &Utf8Path, found: &mut Vec<(String, char, String)>) {
    for entry in at.read_dir_utf8().unwrap() {
        // 1. The path below the root; the workspace's own directory left out.
        let entry = entry.unwrap();
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap().as_str().to_owned();
        if rel == swarmops_ws::state::SWARMWS_DIR {
            continue;
        }

        // 2. What is there, never following a symlink.
        let meta = std::fs::symlink_metadata(path).unwrap();
        if meta.is_dir() {
            walk(root, path, found);
        } else if meta.file_type().is_symlink() {
            let target = std::fs::read_link(path).unwrap();
            found.push((rel, 'l', target.to_str().unwrap().to_owned()));
        } else {
            let text = String::from_utf8(std::fs::read(path).unwrap()).unwrap();
            found.push((rel, kind_of(&meta), text));
        }
    }
}

/// `x` for an executable file, `f` for any other.
#[cfg(unix)]
fn kind_of(meta: &std::fs::Metadata) -> char {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 == 0 {
        'f'
    } else {
        'x'
    }
}

/// `f`: no executable bit where the platform has none.
#[cfg(not(unix))]
fn kind_of(_meta: &std::fs::Metadata) -> char {
    'f'
}

/// The expected [`tree_of`] row of a file.
pub fn file(path: &str, text: &str) -> (String, char, String) {
    (path.to_owned(), 'f', text.to_owned())
}

/// The expected [`tree_of`] row of an executable.
pub fn exec(path: &str, text: &str) -> (String, char, String) {
    (path.to_owned(), 'x', text.to_owned())
}

/// The expected [`tree_of`] row of a symlink.
pub fn link(path: &str, target: &str) -> (String, char, String) {
    (path.to_owned(), 'l', target.to_owned())
}

/// Writes `text` to `path` below `dir`, its directories made: an edit the agent makes.
pub fn write(dir: &Utf8Path, path: &str, text: &str) {
    let abs = dir.join(path);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, text).unwrap();
}

/// Removes the file or symlink at `path` below `dir`.
pub fn remove(dir: &Utf8Path, path: &str) {
    std::fs::remove_file(dir.join(path)).unwrap();
}

/// Makes the file at `path` below `dir` executable, or not.
#[cfg(unix)]
pub fn set_exec(dir: &Utf8Path, path: &str, exec: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    std::fs::set_permissions(dir.join(path), std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Makes a symlink at `path` below `dir` holding `target`, replacing what was there.
#[cfg(unix)]
pub fn symlink(dir: &Utf8Path, path: &str, target: &str) {
    let abs = dir.join(path);
    let _ = std::fs::remove_file(&abs);
    std::os::unix::fs::symlink(target, abs).unwrap();
}

/// Workspace `name`'s changes, every page, as `(path, kind)`: by the index, or every file hashed
/// (`full`).
pub fn changes(ctx: &Ctx<'_>, name: &str, full: bool) -> Vec<(String, WsChangeKind)> {
    let mut found = Vec::new();
    let mut cursor = None;
    loop {
        // 1. A page.
        let mut req = WsStatusReq::new(name.to_owned());
        req.full = full;
        req.limit = 500;
        req.cursor = cursor.take();
        let page = swarmops_ws::status(ctx, req).unwrap();

        // 2. Its changes; the next one, if any.
        found.extend(page.changes.into_iter().map(|c| (c.path, c.kind)));
        if !page.truncated {
            return found;
        }
        cursor = page.cursor;
    }
}

/// `(path, kind)` of a change, as [`changes`] gives one.
pub fn change(path: &str, kind: WsChangeKind) -> (String, WsChangeKind) {
    (path.to_owned(), kind)
}

/// A store that records each call made to it, by operation, and can be told to fail as a crash
/// would or to answer `log` with a made-up history.
pub struct Watched<'a> {
    /// The store the calls go to.
    inner: &'a dyn StoreOps,
    /// The calls made, in order.
    calls: Mutex<Vec<&'static str>>,
    /// The requests `commit` got, and the changesets it recorded, in order.
    commits: Mutex<Vec<(CommitReq, Option<String>)>>,
    /// `commit` records the changeset, then fails as a process killed right after it would.
    pub crash_after_commit: AtomicBool,
    /// `log` of `main`'s head answers this many made-up changesets, none of them the store's,
    /// instead of the store's history; `log` of another changeset reads the store's.
    pub fake_history: Option<u64>,
    /// Where each call to `commit`, `rebase` and `land` is also noted, in order with the file
    /// layers' events (the sweep driver's order checker).
    #[cfg(feature = "test-hooks")]
    pub trace: Option<sweep::Trace>,
}

impl<'a> Watched<'a> {
    /// A store recording the calls made to `inner`.
    pub fn new(inner: &'a dyn StoreOps) -> Watched<'a> {
        Watched {
            inner,
            calls: Mutex::new(Vec::new()),
            commits: Mutex::new(Vec::new()),
            crash_after_commit: AtomicBool::new(false),
            fake_history: None,
            #[cfg(feature = "test-hooks")]
            trace: None,
        }
    }

    /// The calls made so far, in order.
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    /// How many calls of `op` were made.
    pub fn count(&self, op: &str) -> usize {
        self.calls().iter().filter(|call| **call == op).count()
    }

    /// The requests `commit` got, each with the changeset the store recorded for it, if any.
    pub fn commits(&self) -> Vec<(CommitReq, Option<String>)> {
        self.commits.lock().unwrap().clone()
    }

    /// Forgets the calls made so far.
    pub fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }

    /// Records a call of `op`.
    fn saw(&self, op: &'static str) {
        self.calls.lock().unwrap().push(op);
        #[cfg(feature = "test-hooks")]
        if let Some(trace) = &self.trace {
            trace.store(op);
        }
    }

    /// Whether `req` asks for the history of `main`'s head, read without being recorded.
    fn of_main(&self, req: &LogReq) -> bool {
        let head = self
            .inner
            .resolve_rev(ResolveRevReq::new("main"))
            .unwrap()
            .id;
        req.rev.as_ref().is_some_and(|rev| rev.as_str() == head)
    }

    /// The made-up page of `total` changesets that `req` asks for: 64-hex ids counting up from
    /// the cursor, never a changeset of the store.
    fn made_up(total: u64, req: &LogReq) -> LogPage {
        // 1. Where the page starts: after the cursor's entry.
        let start = (req.cursor.as_deref()).map_or(0, |c| u64::from_str_radix(c, 16).unwrap() + 1);
        let end = total.min(start + u64::try_from(req.limit).unwrap());

        // 2. Its entries, and a cursor when more follow.
        let id = |n: u64| format!("{n:064x}");
        let entries = (start..end)
            .map(|n| {
                ChangesetSummary::new(
                    id(n),
                    vec![id(n + 1)],
                    "agent/made-up".to_owned(),
                    0,
                    String::new(),
                )
            })
            .collect();
        let mut out = LogPage::new(entries, end < total);
        out.cursor = (end < total).then(|| id(end - 1));
        out
    }
}

impl StoreOps for Watched<'_> {
    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError> {
        self.saw("repo_info");
        self.inner.repo_info(req)
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        self.saw("commit");
        let done = self.inner.commit(who, req.clone());
        let made = done.as_ref().ok().map(|done| done.changeset.clone());
        self.commits.lock().unwrap().push((req, made));
        let done = done?;
        if self.crash_after_commit.swap(false, Ordering::SeqCst) {
            return Err(OpError::new(ErrorCode::Io, "killed after the commit"));
        }
        Ok(done)
    }

    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError> {
        self.saw("land");
        self.inner.land(who, req)
    }

    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError> {
        self.saw("rebase");
        self.inner.rebase(who, req)
    }

    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError> {
        self.saw("resolve_rev");
        self.inner.resolve_rev(req)
    }

    fn blob_put(&self, who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError> {
        self.saw("blob_put");
        self.inner.blob_put(who, req)
    }

    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError> {
        self.saw("blob_has");
        self.inner.blob_has(req)
    }

    fn blob_get(&self, req: BlobGetReq) -> Result<BlobGetResp, OpError> {
        self.saw("blob_get");
        self.inner.blob_get(req)
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        self.saw("read_file");
        self.inner.read_file(req)
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        self.saw("read_tree");
        self.inner.read_tree(req)
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        self.saw("show");
        self.inner.show(req)
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        self.saw("log");
        match self.fake_history {
            Some(total) if self.of_main(&req) => Ok(Watched::made_up(total, &req)),
            _ => self.inner.log(req),
        }
    }

    fn path_history(&self, req: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
        self.saw("path_history");
        self.inner.path_history(req)
    }

    fn diff(&self, req: DiffReq) -> Result<Diff, OpError> {
        self.saw("diff");
        self.inner.diff(req)
    }

    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError> {
        self.saw("ref_list");
        self.inner.ref_list(req)
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        self.saw("ref_set");
        self.inner.ref_set(who, req)
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        self.saw("ref_delete");
        self.inner.ref_delete(who, req)
    }

    fn op_log(&self, req: OpLogReq) -> Result<OpLogPage, OpError> {
        self.saw("op_log");
        self.inner.op_log(req)
    }

    fn undo(&self, who: &Identity, req: UndoReq) -> Result<UndoResp, OpError> {
        self.saw("undo");
        self.inner.undo(who, req)
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        self.saw("verify");
        self.inner.verify(req)
    }

    fn derive(&self, req: DeriveReq) -> Result<DeriveResp, OpError> {
        self.saw("derive");
        self.inner.derive(req)
    }
}

/// The crash sweeps of the workspace operations (`tests/sweeps.rs`): one driver, five scenarios.
#[cfg(feature = "test-hooks")]
pub mod sweep {
    use std::io;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use camino::Utf8Path;
    use swarmops_api::{
        LogReq, OpError, ResolveRevReq, VerifyReq, WsCheckInput, WsCommitReq, WsLandReq, WsListReq,
        WsRebaseAction, WsRebaseReq,
    };
    use swarmops_store::{CrashAtFs, FsMethod, LinkOutcome};
    use swarmops_testkit::child;
    use swarmops_ws::hostfs::{CrashAtHost, HostMeta};
    use swarmops_ws::pending::PendingOp;
    use swarmops_ws::registry::Registry;
    use swarmops_ws::state::SWARMWS_DIR;
    use swarmops_ws::{lock, materialize, pending};

    use super::*;

    /// The variables that make a run of a sweep's test the child that crashes.
    const STORE: &str = "SWARMOPS_WS_TEST_STORE";
    const CUT: &str = "SWARMOPS_WS_TEST_CUT";

    /// How many children run at once.
    const PARALLEL: usize = 8;

    /// The sweeps' workspace.
    const WS: &str = "w";

    /// Longer than a tick of the filesystem's clock, which stamps file mtimes: a file written
    /// this long before a scan has an mtime before the index that scan writes, so the next
    /// status trusts its row instead of hashing it (the racy rule). Without the wait the number
    /// of host calls of a scenario depends on the timing, and a sweep's cuts on it.
    const PAST_THE_CLOCK_TICK: std::time::Duration = std::time::Duration::from_millis(50);

    /// What a sweep drives.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Scenario {
        /// `materialize::update` to a moved `main`: a land's or a rebase's file moves.
        Update,
        /// `ws commit` of a modified, an added and a removed file.
        Commit,
        /// `ws rebase` of a committed change onto a `main` that moved.
        Rebase,
        /// `ws land` of an uncommitted change (committed with a message) onto a moved `main`.
        Land,
        /// `ws land --check`, the check `true`.
        LandChecked,
    }

    impl Scenario {
        /// The name the child gets it by.
        fn tag(self) -> &'static str {
            match self {
                Scenario::Update => "update",
                Scenario::Commit => "commit",
                Scenario::Rebase => "rebase",
                Scenario::Land => "land",
                Scenario::LandChecked => "land_checked",
            }
        }

        /// The scenario `tag` names.
        fn from_tag(tag: &str) -> Scenario {
            [
                Scenario::Update,
                Scenario::Commit,
                Scenario::Rebase,
                Scenario::Land,
                Scenario::LandChecked,
            ]
            .into_iter()
            .find(|s| s.tag() == tag)
            .unwrap_or_else(|| panic!("no scenario {tag}"))
        }
    }

    /// A tree row of [`tree_of`].
    type Row = (String, char, String);

    /// One thing a run did that the write-order rules (WS1 to WS3) speak of.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Event {
        /// A durable file replaced (`atomic_replace`) or removed (`remove`): the method and the
        /// file's name.
        Fs(FsMethod, String),
        /// The host's tree changed: the call and the path.
        Host(&'static str, String),
        /// A call that changes the store: `commit`, `rebase` or `land`.
        Store(&'static str),
    }

    /// The events of a run, in order, from the three layers.
    #[derive(Clone, Default)]
    pub struct Trace(Arc<Mutex<Vec<Event>>>);

    impl Trace {
        /// Notes a store call, if it changes the store.
        pub fn store(&self, op: &'static str) {
            if matches!(op, "commit" | "rebase" | "land") {
                self.push(Event::Store(op));
            }
        }

        /// Notes an event.
        fn push(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }

        /// The events so far.
        pub fn events(&self) -> Vec<Event> {
            self.0.lock().unwrap().clone()
        }
    }

    /// The name of the file `path` ends in.
    fn name_of(path: &Path) -> String {
        path.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }

    /// The durable files' layer, noting each replace and removal.
    struct TraceFs {
        inner: Arc<dyn FsOps>,
        trace: Trace,
    }

    impl FsOps for TraceFs {
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            self.inner.create_dir_all(p)
        }

        fn write_durable_temp(&self, dir: &Path, bytes: &[u8]) -> io::Result<std::path::PathBuf> {
            self.inner.write_durable_temp(dir, bytes)
        }

        fn link_noclobber(&self, from: &Path, to: &Path) -> io::Result<LinkOutcome> {
            self.inner.link_noclobber(from, to)
        }

        fn fsync_dir(&self, p: &Path) -> io::Result<()> {
            self.inner.fsync_dir(p)
        }

        fn read(&self, p: &Path) -> io::Result<Option<Vec<u8>>> {
            self.inner.read(p)
        }

        fn exists(&self, p: &Path) -> io::Result<bool> {
            self.inner.exists(p)
        }

        fn remove(&self, p: &Path) -> io::Result<()> {
            let done = self.inner.remove(p);
            self.trace.push(Event::Fs(FsMethod::Remove, name_of(p)));
            done
        }

        fn append_durable(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
            self.inner.append_durable(path, bytes)
        }

        fn file_len(&self, path: &Path) -> io::Result<u64> {
            self.inner.file_len(path)
        }

        fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
            self.inner.truncate(path, len)
        }

        fn atomic_replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            let done = self.inner.atomic_replace(path, bytes);
            self.trace
                .push(Event::Fs(FsMethod::AtomicReplace, name_of(path)));
            done
        }

        fn sync_data(&self, path: &Path) -> io::Result<()> {
            self.inner.sync_data(path)
        }

        fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
            self.inner.append(path, bytes)
        }

        fn rename_durable(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_durable(from, to)
        }
    }

    /// The host layer, noting each change of the tree.
    struct TraceHost {
        inner: Arc<dyn HostFs>,
        trace: Trace,
        /// The next call of this name fails, once, before it reaches `inner`, as a process
        /// killed right before it would leave things.
        fail: Mutex<Option<&'static str>>,
    }

    impl TraceHost {
        /// A host noting its changes of the tree in `trace`, over `inner`.
        pub fn new(inner: Arc<dyn HostFs>, trace: Trace) -> TraceHost {
            TraceHost {
                inner,
                trace,
                fail: Mutex::new(None),
            }
        }

        /// The next call of `call` fails, once.
        pub fn failing(self, call: &'static str) -> TraceHost {
            *self.fail.lock().unwrap() = Some(call);
            self
        }

        /// Notes `call` on `path`; the failure asked for, if it is this call.
        fn note(&self, call: &'static str, path: &Utf8Path) -> io::Result<()> {
            self.trace.push(Event::Host(call, path.to_string()));
            let mut fail = self.fail.lock().unwrap();
            if *fail == Some(call) {
                *fail = None;
                return Err(io::Error::other("killed"));
            }
            Ok(())
        }
    }

    impl HostFs for TraceHost {
        fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> io::Result<()> {
            self.note("write_file", path)?;
            self.inner.write_file(path, bytes)
        }

        fn symlink(&self, target: &str, path: &Utf8Path) -> io::Result<()> {
            self.note("symlink", path)?;
            self.inner.symlink(target, path)
        }

        fn symlink_metadata(&self, path: &Utf8Path) -> io::Result<Option<HostMeta>> {
            self.inner.symlink_metadata(path)
        }

        fn read_file(&self, path: &Utf8Path) -> io::Result<Vec<u8>> {
            self.inner.read_file(path)
        }

        fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
            self.inner.read_dir(path)
        }

        fn remove_file(&self, path: &Utf8Path) -> io::Result<()> {
            self.note("remove_file", path)?;
            self.inner.remove_file(path)
        }

        fn remove_dir_if_empty(&self, path: &Utf8Path) -> io::Result<bool> {
            self.inner.remove_dir_if_empty(path)
        }

        fn create_dir_all(&self, path: &Utf8Path) -> io::Result<()> {
            self.inner.create_dir_all(path)
        }

        fn rename(&self, from: &Utf8Path, to: &Utf8Path) -> io::Result<()> {
            self.note("rename", to)?;
            self.inner.rename(from, to)
        }

        fn read_link(&self, path: &Utf8Path) -> io::Result<String> {
            self.inner.read_link(path)
        }

        fn set_mode(&self, path: &Utf8Path, mode: u32) -> io::Result<()> {
            self.note("set_mode", path)?;
            self.inner.set_mode(path, mode)
        }
    }

    /// Whether `event` replaces or removes the durable file `name` (`method`).
    fn is_fs(event: &Event, method: FsMethod, name: &str) -> bool {
        matches!(event, Event::Fs(m, n) if *m == method && n == name)
    }

    /// Whether `event` changes the materialised tree itself (not `.swarmws/`).
    fn is_tree_write(event: &Event) -> bool {
        matches!(event, Event::Host(_, path) if !path.contains(SWARMWS_DIR))
    }

    /// The write-order rules of the M5 design's §2.4 over the events of one operation that
    /// changes the store; the broken rule's name in the error.
    ///
    /// - WS1: the pending record is durable before the store call it names.
    /// - WS2: `workspace.json` is replaced only after the store call returned: none before it.
    /// - WS3: the materialisation intent is durable before the first write of the tree and
    ///   removed after `workspace.json` names the new tree (the index is a cache that status
    ///   rebuilds, written by no materialisation: "Open for the controller" in the W10 report).
    pub fn check_order(events: &[Event]) -> Result<(), String> {
        use FsMethod::{AtomicReplace, Remove};

        // 1. WS1 and WS2 against the first store call, if the operation makes one.
        let store = events.iter().position(|e| matches!(e, Event::Store(_)));
        if let Some(store) = store {
            let pending = events
                .iter()
                .position(|e| is_fs(e, AtomicReplace, pending::PENDING_JSON));
            if pending.is_none_or(|p| p >= store) {
                return Err("WS1: the store call came before the pending record".to_owned());
            }
            let state = events
                .iter()
                .position(|e| is_fs(e, AtomicReplace, state::WORKSPACE_JSON));
            if state.is_some_and(|s| s < store) {
                return Err("WS2: workspace.json was replaced before the store call".to_owned());
            }
        }

        // 2. WS3: the intent before the first write of the tree; its removal after the state.
        if let Some(write) = events.iter().position(is_tree_write) {
            let intent = events
                .iter()
                .position(|e| is_fs(e, AtomicReplace, pending::MATERIALIZE_JSON));
            if intent.is_none_or(|i| i >= write) {
                return Err("WS3: the tree was written before the intent was durable".to_owned());
            }
            let state = events
                .iter()
                .rposition(|e| is_fs(e, AtomicReplace, state::WORKSPACE_JSON));
            let removed = events
                .iter()
                .rposition(|e| is_fs(e, Remove, pending::MATERIALIZE_JSON));
            let ordered = matches!((state, removed), (Some(s), Some(r)) if s < r);
            if !ordered {
                return Err(
                    "WS3: the intent was removed before the state named the tree".to_owned(),
                );
            }
        }
        Ok(())
    }

    /// `events` with the first event `moved` picks put just after the first event `after` picks:
    /// a rule deliberately broken, for the self-check of the order checker.
    pub fn reordered(
        events: &[Event],
        moved: impl Fn(&Event) -> bool,
        after: impl Fn(&Event) -> bool,
    ) -> Vec<Event> {
        let mut out = events.to_vec();
        let from = out.iter().position(moved).expect("an event to move");
        let item = out.remove(from);
        let to = out.iter().position(after).expect("an event to follow");
        out.insert(to + 1, item);
        out
    }

    /// The pending record's replace, as [`reordered`] picks it.
    pub fn pending_write(e: &Event) -> bool {
        is_fs(e, FsMethod::AtomicReplace, pending::PENDING_JSON)
    }

    /// The first store call, as [`reordered`] picks it.
    pub fn store_call(e: &Event) -> bool {
        matches!(e, Event::Store(_))
    }

    /// The state file's replace, as [`reordered`] picks it.
    pub fn state_write(e: &Event) -> bool {
        is_fs(e, FsMethod::AtomicReplace, state::WORKSPACE_JSON)
    }

    /// The intent's removal, as [`reordered`] picks it.
    pub fn intent_removal(e: &Event) -> bool {
        is_fs(e, FsMethod::Remove, pending::MATERIALIZE_JSON)
    }

    /// The intent's replace, as [`reordered`] picks it.
    pub fn intent_write(e: &Event) -> bool {
        is_fs(e, FsMethod::AtomicReplace, pending::MATERIALIZE_JSON)
    }

    /// The first write of the tree, as [`reordered`] picks it.
    pub fn tree_write(e: &Event) -> bool {
        is_tree_write(e)
    }

    /// What a scenario leaves, compared between a run that never crashed and one that did.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Outcome {
        /// The workspace's files.
        pub files: Vec<Row>,
        /// The files of the tree its head names.
        pub head_tree: Vec<Row>,
        /// The files of the tree `main` names.
        pub main_tree: Vec<Row>,
        /// The changesets reachable from `main`, and from the workspace's head.
        pub chains: (usize, usize),
        /// The workspace's changes, by `--full`.
        pub changes: Vec<(String, WsChangeKind)>,
    }

    /// The store and the workspace the scenario starts from.
    fn prepare(f: &Fixture, scenario: Scenario) {
        // 1. A base of seven files in three directories, and the workspace of it.
        let base = f.commit(
            None,
            &[
                Put::File("src/a.rs", "a\n"),
                Put::File("src/b.rs", "b\n"),
                Put::File("src/c.rs", "c\n"),
                Put::File("docs/x.md", "x\n"),
                Put::File("docs/y.md", "y\n"),
                Put::Exec("tools/run", "#!/bin/sh\n"),
                Put::Link("tools/current", "run"),
            ],
        );
        f.create(WS, &base, &[]);
        let dir = f.beside(WS);

        // 2. Each scenario's start.
        match scenario {
            Scenario::Update => {
                f.commit(
                    Some(&base),
                    &[
                        Put::File("src/a.rs", "a2\n"),
                        Put::Gone("docs/x.md"),
                        Put::File("new/deep/z.rs", "z\n"),
                        Put::Exec("tools/run", "#!/bin/sh\nexit 0\n"),
                    ],
                );
            }
            Scenario::Commit => {
                write(&dir, "src/a.rs", "edit\n");
                write(&dir, "new/z.rs", "z\n");
                remove(&dir, "docs/x.md");
            }
            Scenario::Rebase | Scenario::Land | Scenario::LandChecked => {
                // A commit of the workspace's own, and `main` moved on another file. The wait
                // makes every file older than the commit's index, so each run of the scenario
                // makes the same calls.
                write(&dir, "src/a.rs", "mine\n");
                std::thread::sleep(PAST_THE_CLOCK_TICK);
                let mut req = WsCommitReq::new(WS.to_owned(), "mine".to_owned());
                req.status_after = StatusAfter::None;
                swarmops_ws::commit(&f.ctx(), req).unwrap();
                f.commit(Some(&base), &[Put::File("docs/y.md", "y2\n")]);
                if scenario != Scenario::Rebase {
                    write(&dir, "src/b.rs", "uncommitted\n");
                }
            }
        }
    }

    /// An update whose process dies right after it wrote the file `tools/run` and before it set
    /// the executable bit, as the old file was executable: the next open finishes it, and the
    /// tree is the one an update that was not cut leaves.
    pub fn update_cut_before_the_mode() {
        // 1. The update, its host failing at the first `set_mode`.
        let f = Fixture::new();
        prepare(&f, Scenario::Update);
        let host =
            TraceHost::new(Arc::new(RealHost::default()), Trace::default()).failing("set_mode");
        let ctx = f.ctx_on(Arc::new(host));
        assert!(act(&ctx, Scenario::Update).is_err());
        let dir = f.beside(WS);
        assert!(
            pending::read_intent(&RealFs::new(), &dir)
                .unwrap()
                .is_some()
        );

        // 2. The next open finishes it.
        let ctx = f.ctx();
        Fixture::open(&ctx, WS);
        assert!(
            pending::read_intent(&RealFs::new(), &dir)
                .unwrap()
                .is_none()
        );
        assert_eq!(outcome(&f, &ctx), reference(Scenario::Update).outcome);
    }

    /// The same cut update, then the agent makes the plain file `src/a.rs` (already written with
    /// its new bytes) executable: the next open refuses that edit with `E_WS_DIRTY`, names only
    /// it, and keeps its mode; `tools/run`, left without its bit by the cut, is the update's own.
    #[cfg(unix)]
    pub fn update_cut_then_an_agent_mode_edit() {
        // 1. The update, its host failing at the first `set_mode`.
        let f = Fixture::new();
        prepare(&f, Scenario::Update);
        let host =
            TraceHost::new(Arc::new(RealHost::default()), Trace::default()).failing("set_mode");
        let ctx = f.ctx_on(Arc::new(host));
        assert!(act(&ctx, Scenario::Update).is_err());
        let dir = f.beside(WS);

        // 2. The agent's edit: the mode of a file the cut update wrote.
        set_exec(&dir, "src/a.rs", true);

        // 3. Refused, naming that file alone, and its mode kept.
        let refused = Workspace::open(&f.ctx(), Locate::Name(WS.to_owned())).unwrap_err();
        assert_eq!(refused.code, "E_WS_DIRTY", "{refused:?}");
        assert!(refused.message.contains("\"src/a.rs\""), "{refused:?}");
        assert!(!refused.message.contains("\"tools/run\""), "{refused:?}");
        assert!(tree_of(&dir).contains(&exec("src/a.rs", "a2\n")));
    }

    /// The scenario's operation, once, through `ctx`.
    fn act(ctx: &Ctx<'_>, scenario: Scenario) -> Result<(), OpError> {
        let main = ctx.store.resolve_rev(ResolveRevReq::new("main"))?.id;
        match scenario {
            Scenario::Update => {
                let mut ws = Fixture::open(ctx, WS);
                let _held = lock::acquire(&ws.dir)?;
                materialize::update(ctx, &mut ws, &main).map(|_| ())
            }
            Scenario::Commit => {
                let mut req = WsCommitReq::new(WS.to_owned(), "sweep".to_owned());
                req.status_after = StatusAfter::None;
                swarmops_ws::commit(ctx, req).map(|_| ())
            }
            Scenario::Rebase => {
                let mut req = WsRebaseReq::new(WS.to_owned());
                req.action = WsRebaseAction::Start;
                req.status_after = StatusAfter::None;
                swarmops_ws::rebase(ctx, req).map(|_| ())
            }
            Scenario::Land | Scenario::LandChecked => {
                let check = (scenario == Scenario::LandChecked).then(|| {
                    let mut check = WsCheckInput::default();
                    check.argv = Some(vec!["true".to_owned()]);
                    check.timeout_s = 60;
                    check
                });
                let mut req = WsLandReq::new(WS.to_owned());
                req.message = Some("sweep land".to_owned());
                req.check = check;
                req.max_attempts = 4;
                req.status_after = StatusAfter::None;
                swarmops_ws::land(ctx, req).map(|_| ())
            }
        }
    }

    /// How many changesets `rev` reaches along first parents.
    fn chain(f: &Fixture, rev: &str) -> usize {
        let mut req = LogReq::default();
        req.rev = Some(rev.into());
        req.limit = 1000;
        f.ops.log(req).unwrap().entries.len()
    }

    /// What the scenario left in `f`, read through `ctx`.
    fn outcome(f: &Fixture, ctx: &Ctx<'_>) -> Outcome {
        let head = Fixture::open(ctx, WS).file.head;
        f.create_in(ctx, "probe-head", &head, &[]).unwrap();
        f.create_in(ctx, "probe-main", "main", &[]).unwrap();
        Outcome {
            files: tree_of(&f.beside(WS)),
            head_tree: tree_of(&f.beside("probe-head")),
            main_tree: tree_of(&f.beside("probe-main")),
            chains: (chain(f, "main"), chain(f, &head)),
            changes: changes(ctx, WS, true),
        }
    }

    /// A context on `ops`, the store at `root`, over `host` and `fs`.
    fn ctx_on<'a>(
        ops: &'a LocalOps,
        root: &str,
        host: Arc<dyn HostFs>,
        fs: Arc<dyn FsOps>,
    ) -> Ctx<'a> {
        Ctx::new(
            ops,
            root.into(),
            Some(Identity::parse(AGENT).unwrap()),
            fs,
            host,
        )
    }

    /// What a run that never crashes shows: its calls on each layer, its events, its outcome.
    pub struct Reference {
        /// Calls of the host layer.
        pub host_calls: u64,
        /// Calls of the durable files' layer.
        pub fs_calls: u64,
        /// The store-changing events, in order.
        pub events: Vec<Event>,
        /// What it left.
        pub outcome: Outcome,
    }

    /// The scenario run once with nothing cut, under counting doubles and the trace.
    pub fn reference(scenario: Scenario) -> Reference {
        // 1. The scenario's start, a store watched so that its calls enter the trace.
        let f = Fixture::new();
        prepare(&f, scenario);
        let trace = Trace::default();
        let mut watched = Watched::new(&f.ops);
        watched.trace = Some(trace.clone());

        // 2. Run over counting doubles under the trace.
        let counting_host = Arc::new(CrashAtHost::new(RealHost::default(), 0));
        let counting_fs = Arc::new(CrashAtFs::new(RealFs::new(), 0));
        let host = TraceHost::new(counting_host.clone(), trace.clone());
        let fs = TraceFs {
            inner: counting_fs.clone(),
            trace: trace.clone(),
        };
        let mut ctx = f.ctx_over(&watched);
        ctx.host = Arc::new(host);
        ctx.fs = Arc::new(fs);

        act(&ctx, scenario).unwrap();

        // 3. The outcome through the plain context.
        let outcome = outcome(&f, &f.ctx());
        Reference {
            host_calls: counting_host.calls(),
            fs_calls: counting_fs.calls(),
            events: trace.events(),
            outcome,
        }
    }

    /// Kills a create-free scenario at each call (every `stride`-th of the host's, and its last;
    /// every one of the durable files') in a child process, a run of `test`, then recovers it;
    /// in that child, runs the scenario over the layer that aborts and dies.
    pub fn run(test: &str, scenario: Scenario, stride: u64) {
        // 1. In the child: the scenario, over a layer that aborts at call k.
        if let (Ok(root), Ok(cut)) = (std::env::var(STORE), std::env::var(CUT)) {
            crash(&root, &cut);
            return;
        }

        // 2. How many calls the scenario makes, and what it leaves when nothing is cut.
        let expected = reference(scenario);
        let (host_calls, fs_calls) = (expected.host_calls, expected.fs_calls);
        assert!(host_calls > 0 && fs_calls > 0, "{host_calls} {fs_calls}");

        // 3. A child killed at each cut, then recovered: eight at a time.
        let cuts: Vec<(&str, u64)> = (1..=host_calls)
            .filter(|k| (k - 1) % stride == 0 || *k == host_calls)
            .map(|k| ("host", k))
            .chain((1..=fs_calls).map(|k| ("fs", k)))
            .collect();
        eprintln!(
            "sweep {}: {} cuts of {host_calls} host and {fs_calls} fs calls",
            scenario.tag(),
            cuts.len()
        );
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..PARALLEL {
                scope.spawn(|| {
                    while let Some(&cut) = cuts.get(next.fetch_add(1, Ordering::SeqCst)) {
                        cut_and_recover(test, scenario, cut, &expected.outcome);
                    }
                });
            }
        });
    }

    /// The child's part: the scenario on the store at `root`, over the layer `cut`
    /// (`<scenario>:<layer>:<k>`) names, aborting at its call `k`.
    fn crash(root: &str, cut: &str) {
        // 1. The cut.
        let mut parts = cut.splitn(3, ':');
        let scenario = Scenario::from_tag(parts.next().unwrap());
        let layer = parts.next().unwrap();
        let k = parts.next().unwrap().parse().unwrap();

        // 2. The scenario over the layer that aborts.
        let ops = LocalOps::open(Utf8Path::new(root)).unwrap();
        let (host, fs): (Arc<dyn HostFs>, Arc<dyn FsOps>) = match layer {
            "host" => (
                Arc::new(CrashAtHost::new(RealHost::default(), k)),
                Arc::new(RealFs::new()),
            ),
            _ => (
                Arc::new(RealHost::default()),
                Arc::new(CrashAtFs::new(RealFs::new(), k)),
            ),
        };
        let ctx = ctx_on(&ops, root, host, fs);
        let _ = act(&ctx, scenario);
    }

    /// A store of its own made ready, a child (a run of `test`) killed at call `k` of `layer`,
    /// then the scenario recovered and checked against `expected`.
    fn cut_and_recover(test: &str, scenario: Scenario, cut: (&str, u64), expected: &Outcome) {
        // 1. A store and workspace of this cut's own, and the child killed in it.
        let (layer, k) = cut;
        let f = Fixture::new();
        prepare(&f, scenario);
        let out = child::run(
            child::this_test(test)
                .env(STORE, f.root.as_str())
                .env(CUT, format!("{}:{layer}:{k}", scenario.tag())),
        );
        let name = format!("{} {layer} {k}", scenario.tag());
        assert!(!out.status.success(), "{name}: the child was not killed");

        // 2. Opened: a materialisation it left is finished.
        let ctx = f.ctx_with(
            Arc::new(RealHost::default()),
            swarmops_testkit::faults::checked(RealFs::new()),
        );
        let dir = f.beside(WS);
        let head = Fixture::open(&ctx, WS).file.head;
        assert!(
            pending::read_intent(&RealFs::new(), &dir)
                .unwrap()
                .is_none(),
            "{name}: an intent is left after the open"
        );

        // 3. A pending record on this head names the call to run again first: the land's own
        //    commit of the edits, when the child was cut inside it. Then the operation again:
        //    it finishes; where it was done already, only the refusal that says so is allowed,
        //    and not with a record of this head to resume.
        let record = pending::read_pending(&RealFs::new(), &dir).unwrap();
        let live = record.as_ref().filter(|r| r.head == head);
        if live.is_some_and(|r| r.op == PendingOp::Commit) && scenario != Scenario::Commit {
            commit_edits(&ctx, "sweep land").unwrap_or_else(|e| panic!("{name}: {e:?}"));
        }
        match act(&ctx, scenario) {
            Ok(()) => {}
            Err(e) if live.is_none() && is_done(&e) => {}
            Err(e) => panic!("{name}: the run again failed: {e:?} (pending {record:?})"),
        }

        // 4. Nothing half done (a record left is of an older head, which the next call drops),
        //    and the store and the workspace as the uncut run left them.
        let head = Fixture::open(&ctx, WS).file.head;
        let left = pending::read_pending(&RealFs::new(), &dir).unwrap();
        assert!(left.is_none_or(|r| r.head != head), "{name}: pending");
        assert!(
            pending::read_intent(&RealFs::new(), &dir)
                .unwrap()
                .is_none(),
            "{name}: intent"
        );
        let mut req = VerifyReq::default();
        req.deep = true;
        let report = f.ops.verify(req).unwrap();
        assert!(report.problems.is_empty(), "{name}: {:?}", report.problems);
        assert_eq!(changes(&ctx, WS, false), changes(&ctx, WS, true), "{name}");
        registry_is_consistent(&f, &ctx, &name);
        assert_eq!(&outcome(&f, &ctx), expected, "{name}");
    }

    /// `ws commit` of every edit of the sweeps' workspace.
    fn commit_edits(ctx: &Ctx<'_>, message: &str) -> Result<(), OpError> {
        let mut req = WsCommitReq::new(WS.to_owned(), message.to_owned());
        req.status_after = StatusAfter::None;
        swarmops_ws::commit(ctx, req).map(|_| ())
    }

    /// Whether `e` says the operation had been done already.
    fn is_done(e: &OpError) -> bool {
        matches!(e.code.as_str(), "E_EMPTY_COMMIT" | "E_NOTHING_TO_LAND")
    }

    /// The registry holds the one workspace, listed and not an orphan, and no file but its entry.
    fn registry_is_consistent(f: &Fixture, ctx: &Ctx<'_>, name: &str) {
        let mut req = WsListReq::default();
        req.check = true;
        let listed = swarmops_ws::list(ctx, req).unwrap();
        let names: Vec<(&str, bool)> = (listed.workspaces.iter())
            .map(|w| (w.name.as_str(), w.orphan))
            .collect();
        assert_eq!(names, [(WS, false)], "{name}");
        let entries = Registry::open(&f.root, Arc::new(RealFs::new()))
            .list()
            .unwrap();
        assert_eq!(entries.len(), 1, "{name}");
    }
}
