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
use swarmops_ws::state::{self, Volume, WorkspaceFile};
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
        self.ctx_with(host, Arc::new(RealFs))
    }

    /// What the workspace operations run with, over the host `host` and the durable files' `fs`.
    pub fn ctx_with(&self, host: Arc<dyn HostFs>, fs: Arc<dyn FsOps>) -> Ctx<'_> {
        Ctx {
            store: &self.ops,
            store_root: self.root.clone(),
            who: Some(Identity::parse(AGENT).unwrap()),
            fs,
            host,
        }
    }

    /// What the workspace operations run with, over the real host, with `store` in place of the
    /// fixture's own (a [`Watched`] one over it).
    pub fn ctx_over<'s>(&'s self, store: &'s dyn StoreOps) -> Ctx<'s> {
        Ctx {
            store,
            ..self.ctx()
        }
    }

    /// What the workspace operations run with, over the real host.
    pub fn ctx(&self) -> Ctx<'_> {
        self.ctx_on(Arc::new(RealHost))
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
                        return ChangeInput {
                            path: path.to_owned(),
                            op: ChangeOp::Delete,
                            content: None,
                            mode: None,
                        };
                    }
                };
                ChangeInput {
                    path: path.to_owned(),
                    op: ChangeOp::Put,
                    content: Some(ContentInput::Inline(text.to_owned())),
                    mode: Some(mode),
                }
            })
            .collect();

        // 2. Recorded on the parent, `main` moved along.
        let req = CommitReq {
            parent: parent.map(Into::into),
            changes,
            message: "fixture".to_owned(),
            advance: Some("main".to_owned()),
            idempotency_key: None,
            allow_empty: false,
        };
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
        let req = WsCreateReq {
            name: name.to_owned(),
            path: Some(self.beside(name).to_string()),
            base: base.into(),
            prefixes: prefixes.iter().map(|p| (*p).to_owned()).collect(),
            all: prefixes.is_empty(),
            from: None,
            session: None,
            status_after: StatusAfter::None,
        };
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
        let file = WorkspaceFile {
            volume,
            ..state::read(&RealFs, &dir).unwrap()
        };
        state::replace(&RealFs, &dir, &file).unwrap();
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
        let req = WsStatusReq {
            workspace: name.to_owned(),
            paths: Vec::new(),
            full,
            limit: 500,
            cursor: cursor.take(),
        };
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
    /// `log` answers this many made-up changesets, none of them the store's, instead of the
    /// store's history.
    pub fake_history: Option<u64>,
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
            .map(|n| ChangesetSummary {
                id: id(n),
                parents: vec![id(n + 1)],
                author_agent: "agent/made-up".to_owned(),
                created_at: 0,
                title: String::new(),
            })
            .collect();
        LogPage {
            entries,
            truncated: end < total,
            cursor: (end < total).then(|| id(end - 1)),
        }
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
            Some(total) => Ok(Watched::made_up(total, &req)),
            None => self.inner.log(req),
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
