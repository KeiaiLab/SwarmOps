//! What `serve.git` advertises is the committed snapshot's export (M14 design §4): before each
//! advertisement, every served store ref exported into it, and a ref the store no longer has
//! removed.
//!
//! ```text
//! 1 serve.lock (bounded wait, E_BUSY)   4 each served ref: export(force) into serve.git,
//! 2 the committed snapshot's refs          a no-op where it names the export already;
//!   → git refs (refmap::to_git)            again on a concurrent move, 8 times at most
//! 3 git refs no store ref maps to:      5 git gc --auto once objects were written
//!   deleted by compare-and-swap
//! ```
//!
//! The export is `git export`'s own ([`swarmops_git::export()`]), forced: `serve.git` is
//! SwarmOps' cache, and a store ref that moved back (`op undo`) is advertised where it is. Each
//! export reads its head in a committed snapshot of its own, so nothing a power loss or a failed
//! sync's cut could take back is advertised. A store name git takes no ref for, and one under a
//! served branch's name (`a/b` beside `a`, which git's files cannot hold both of), is left out
//! with a warning.

use std::collections::BTreeMap;
use std::time::Duration;

use swarmops_api::{ErrorCode, OpError};
use swarmops_git::serve::refmap::to_git;
use swarmops_git::serve::repo::{ServeError, ServeLock, ServeRepo};
use swarmops_git::transport::{GitCli, Transport as _};
use swarmops_git::{
    BranchUpdate, DivergedReason, ExportError, ExportReport, ExportRequest, GixSink, SinkError,
    export,
};
use swarmops_store::{RefKind, ref_kind};

use super::not_built;
use crate::LocalOps;
use crate::error;

/// How many times a refresh reads a ref again after its compare-and-swap in `serve.git` failed.
pub const SERVE_REFRESH_ATTEMPTS: u32 = 8;

/// How long a refresh waits for `git/serve.lock`: the store's writer lock wait (the limits
/// page's "Writer lock wait"), never more.
const SERVE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Where a branch and a tag are in git.
const HEADS: &str = "refs/heads/";
const TAGS: &str = "refs/tags/";

/// The name the texts of a refresh give the serving repository: never its host path.
const SERVE_GIT: &str = "serve.git";

/// Which refs a refresh exports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Scope {
    /// Every ref served.
    All,
    /// These store refs alone.
    Refs(Vec<String>),
}

/// What a refresh did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Refreshed {
    /// Refs moved in `serve.git`, or made.
    pub moved: u64,
    /// Refs removed from it.
    pub removed: u64,
    /// Objects the exports wrote: 0 when every ref named its export already.
    pub objects: u64,
    /// What did not stop it, one line each: a store ref left out, a dead process's git lock file
    /// removed, a `git gc --auto` that failed.
    pub warnings: Vec<String>,
}

/// A served store ref: its name and kind.
type ServedRef = (String, RefKind);

/// Exports `scope`'s refs of the store `ops` serves into `repo`, from its committed snapshot.
pub fn refresh(ops: &LocalOps, repo: &ServeRepo, scope: &Scope) -> Result<Refreshed, OpError> {
    let lock = repo.lock(SERVE_LOCK_WAIT).map_err(serve_error)?;
    let mut refreshed = Refreshed::default();

    // 1. What a dead process left in serve.git, cleared before any ref moves.
    let swept = repo.sweep_git_locks(&lock).map_err(serve_error)?;
    let swept = swept
        .into_iter()
        .map(|file| format!("git_lock_removed: {file}"));
    refreshed.warnings.extend(swept);

    // 2. The store's refs, committed, mapped to the git refs they are served as.
    let (read, _) = ops.repo.snapshot_committed().map_err(error::op)?;
    let listed = read.list_refs().map_err(error::op)?;
    drop(read);
    let names = listed.into_iter().map(|(name, _)| name);
    let wanted = served(names.filter(|name| in_scope(scope, name)), &mut refreshed);

    // 3. Each git ref in scope no store ref is served as: deleted, before an export needs its
    // place (`a` gone, `a/b` made).
    let in_scope_git = |full: &str| match scope {
        Scope::All => true,
        Scope::Refs(names) => names
            .iter()
            .any(|name| git_ref_of(name).as_deref() == Some(full)),
    };
    for (full, oid) in repo.refs().map_err(serve_error)? {
        if in_scope_git(&full) && !wanted.contains_key(&full) {
            delete(repo, &lock, &full, oid)?;
            refreshed.removed += 1;
        }
    }

    // 4. Each served ref exported; one the store deleted meanwhile is deleted in git too.
    for (full, (store, kind)) in &wanted {
        let Some(report) = export_one(ops, repo, full, store, *kind)? else {
            let current = repo.refs().map_err(serve_error)?;
            if let Some(oid) = current.get(full) {
                delete(repo, &lock, full, *oid)?;
                refreshed.removed += 1;
            }
            continue;
        };
        refreshed.moved += u64::from(report.branch_moved);
        refreshed.objects += report.objects_written;
        refreshed.warnings.extend(report.warnings);
    }

    // 5. git's own housekeeping, once objects were written: a failure is a warning.
    if refreshed.objects > 0 {
        let gc = GitCli::find().and_then(|git| git.gc_auto(repo.git_dir().as_std_path()));
        if let Err(e) = gc {
            let failed = error::transport_error(e, SERVE_GIT, error::Phase::Push);
            refreshed
                .warnings
                .push(format!("gc_auto_failed: {}", failed.message));
        }
    }
    Ok(refreshed)
}

/// The served store refs among `names`, by the git ref each is served as. A remote-tracking ref
/// is passed over; a name git takes no ref for, or one under a served branch's name, is left out
/// with a warning in `refreshed`.
fn served(
    names: impl Iterator<Item = String>,
    refreshed: &mut Refreshed,
) -> BTreeMap<String, ServedRef> {
    let mut mapped = BTreeMap::new();
    for name in names {
        let Some(kind) = ref_kind(&name) else {
            continue;
        };
        if kind == RefKind::RemoteTracking {
            continue;
        }
        match to_git(kind, &name) {
            Some(full) => {
                mapped.insert(full, (name, kind));
            }
            None => refreshed
                .warnings
                .push(format!("not_served: {name}: git takes no ref of that name")),
        }
    }

    // A ref whose name has a served ref's name as a directory: git's files hold one of the two.
    // In name order a directory's ref comes first, and is kept.
    let mut kept: BTreeMap<String, ServedRef> = BTreeMap::new();
    for (full, served) in mapped {
        let under = ancestors(&full).find(|dir| kept.contains_key(*dir));
        if let Some(dir) = under {
            refreshed.warnings.push(format!(
                "not_served: {}: {dir} is served, and git holds no ref under a ref's name",
                served.0
            ));
            continue;
        }
        kept.insert(full, served);
    }
    kept
}

/// Each proper prefix of `full` that ends before a `/`: `refs/heads/a/b` gives `refs`,
/// `refs/heads`, `refs/heads/a`.
fn ancestors(full: &str) -> impl Iterator<Item = &str> {
    let cuts = full.match_indices('/').map(|(at, _)| at);
    cuts.map(move |at| &full[..at])
}

/// Whether the store ref `name` is one `scope` names.
fn in_scope(scope: &Scope, name: &str) -> bool {
    match scope {
        Scope::Refs(names) => names.iter().any(|named| named == name),
        _ => true,
    }
}

/// The git ref the store ref `name` is served as, if any.
fn git_ref_of(name: &str) -> Option<String> {
    to_git(ref_kind(name)?, name)
}

/// Exports the store ref `store` of kind `kind` into `full` of `repo`, forced, by
/// compare-and-swap, again while another writer moves it, [`SERVE_REFRESH_ATTEMPTS`] times at
/// most; `None` when the store no longer has the ref.
fn export_one(
    ops: &LocalOps,
    repo: &ServeRepo,
    full: &str,
    store: &str,
    kind: RefKind,
) -> Result<Option<ExportReport>, OpError> {
    let short = full
        .strip_prefix(HEADS)
        .or_else(|| full.strip_prefix(TAGS))
        .unwrap_or(full);
    let mut req = match kind {
        RefKind::Tag => ExportRequest::tag(store, short),
        _ => ExportRequest::new(store, short),
    };
    req.force = true;
    req.memo_key.clone_from(&ops.memo_key);
    let failed = |e: ExportError| error::git_export_error(e, SERVE_GIT, store, short);

    // Each attempt opens serve.git afresh: what gix cached of its refs is not read again.
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut sink = GixSink::open(repo.git_dir().as_std_path())
            .map_err(|e| serve_error(ServeError::Sink(e)))?;
        match export(&ops.repo, &mut sink, &req) {
            Ok(report) => return Ok(Some(report)),
            Err(ExportError::SourceRef(_)) => return Ok(None),
            Err(ExportError::Diverged {
                reason: DivergedReason::MovedDuringExport { .. },
                ..
            }) if attempt < SERVE_REFRESH_ATTEMPTS => {}
            Err(e) => return Err(failed(e)),
        }
    }
}

/// Deletes `full` from `repo`, where it names `oid`.
fn delete(
    repo: &ServeRepo,
    lock: &ServeLock,
    full: &str,
    oid: swarmops_git::GitOid,
) -> Result<(), OpError> {
    match repo
        .set_ref(lock, full, Some(oid), None)
        .map_err(serve_error)?
    {
        BranchUpdate::Applied => Ok(()),
        BranchUpdate::Mismatch { actual } => {
            let found = actual.map_or_else(|| "nothing".to_owned(), |oid| oid.to_string());
            let reason =
                format!("{full} names {found}, not {oid}: something other than SwarmOps moved it");
            Err(serve_error(ServeError::Sink(SinkError::Write {
                what: full.to_owned(),
                reason,
            })))
        }
        _ => Err(error::error(ErrorCode::Internal, "an unknown ref update")),
    }
}

/// The operations-spec error of a failure of the serving repository: `E_BUSY` for its lock
/// (`lock: "git_serve"`) and for a git lock file in it (`lock: "git_serve_file"`), `E_IO` for a
/// file of `git/`, `E_GIT_WRITE` (`reason: "serve"`) for the rest of `serve.git`.
pub(crate) fn serve_error(e: ServeError) -> OpError {
    let message = e.to_string();
    let again =
        "serve.git is a cache: connect again; one git cannot open is set aside and made again";
    match e {
        ServeError::Busy => error::error(ErrorCode::Busy, message)
            .with_details(error::details([("lock", "git_serve".into())]))
            .with_hint("connect again once the other connection's refresh has ended"),
        ServeError::Sink(SinkError::RefLocked { lock }) => error::error(ErrorCode::Busy, message)
            .with_details(error::details([
                ("lock", "git_serve_file".into()),
                ("file", lock.into()),
            ]))
            .with_hint(
                "connect again in a few minutes: a dead process's lock file is cleared then",
            ),
        ServeError::Io { what, .. } => error::error(ErrorCode::Io, message)
            .with_details(error::details([(
                "store_file",
                format!("git/{what}").into(),
            )]))
            .with_hint(again),
        ServeError::Sink(_) => error::error(ErrorCode::GitWrite, message)
            .with_details(error::details([("reason", "serve".into())]))
            .with_hint(again),
        ServeError::NotBuilt => not_built("serve.git"),
        _ => error::error(ErrorCode::Internal, message),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use camino::{Utf8Path, Utf8PathBuf};
    use swarmops_api::{
        ChangeInput, ChangeOp, CommitReq, ContentInput, Identity, OpLogReq, RefDeleteReq,
        RefSetReq, StoreOps as _, UndoReq,
    };
    use swarmops_git::serve::repo::ServeRepo;
    use swarmops_git::{ExportRequest, GitOid, GixSink, export};
    use swarmops_repo::{LocalRepo, RepoError, StoreError};
    use swarmops_store::{Layout, RealFs, StoreFormat};
    use swarmops_testkit::fs::HoldThenFail;
    use tempfile::TempDir;

    use super::{Scope, refresh};
    use crate::LocalOps;

    /// The refs the served store holds, and the git refs they are served as.
    const SERVED: [(&str, &str); 4] = [
        ("main", "refs/heads/main"),
        ("release/1", "refs/heads/release/1"),
        ("feature/x", "refs/heads/feature/x"),
        ("tags/v1", "refs/tags/v1"),
    ];

    /// The agent every write of these tests records.
    const AGENT: &str = "agent/worker-17";

    /// A FORMAT 2 store holding [`SERVED`]'s refs, and its serving repository.
    struct Site {
        dir: TempDir,
        root: Utf8PathBuf,
        ops: LocalOps,
        repo: ServeRepo,
        who: Identity,
    }

    impl Site {
        /// `main` two commits deep, `release/1` and `tags/v1` at its first, `feature/x` one
        /// commit past `main`.
        fn new() -> Site {
            let dir = tempfile::tempdir().expect("a temp dir");
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).expect("UTF-8");
            let (ops, _) = LocalOps::init_as(&root, "acme", Some(2)).expect("init");
            let repo = ServeRepo::open(&root).expect("open serve.git");
            let who = Identity::parse(AGENT).expect("an identity");
            let site = Site {
                dir,
                root,
                ops,
                repo,
                who,
            };
            let first = site.commit("a", Some("main"));
            site.create("release/1", &first);
            site.create("tags/v1", &first);
            site.commit("b", Some("main"));
            let ahead = site.commit("c", None);
            site.create("feature/x", &ahead);
            site
        }

        /// Commits a file `path` on `main`'s head, moving `advance` when given; its id.
        fn commit(&self, path: &str, advance: Option<&str>) -> String {
            let mut change = ChangeInput::new(path.to_owned(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline(path.to_owned()));
            let mut req = CommitReq::new(vec![change], path);
            req.advance = advance.map(str::to_owned);
            self.ops.commit(&self.who, req).expect("commit").changeset
        }

        /// Makes the ref `name` at the changeset `id`.
        fn create(&self, name: &str, id: &str) {
            let mut req = RefSetReq::new(name, id);
            req.create = true;
            self.ops.ref_set(&self.who, req).expect("ref set");
        }

        /// The refs of `serve.git`.
        fn served(&self) -> BTreeMap<String, GitOid> {
            self.repo.refs().expect("refs of serve.git")
        }

        /// The commit `git export` gives the store ref `name`, into an empty repository of its
        /// own.
        fn exported(&self, name: &str) -> GitOid {
            let path = self
                .dir
                .path()
                .join(format!("export-{}.git", name.replace('/', "-")));
            if !path.exists() {
                GixSink::create_bare(&path).expect("an empty repository");
            }
            let mut sink = GixSink::open(&path).expect("open it");
            let mut req = ExportRequest::new(name, "out");
            req.force = true;
            let report = export(&self.ops.repo, &mut sink, &req).expect("export");
            report.head_oid
        }
    }

    /// Every file of the directory `dir`, below it, with its length and modification time.
    fn files(dir: &Path) -> BTreeSet<(String, u64, std::time::SystemTime)> {
        let mut found = BTreeSet::new();
        let mut todo = vec![dir.to_owned()];
        while let Some(at) = todo.pop() {
            for entry in std::fs::read_dir(&at).expect("read a directory") {
                let entry = entry.expect("an entry");
                let meta = entry.metadata().expect("its metadata");
                if meta.is_dir() {
                    todo.push(entry.path());
                    continue;
                }
                let name = entry.path().display().to_string();
                found.insert((name, meta.len(), meta.modified().expect("mtime")));
            }
        }
        found
    }

    /// After a refresh, `serve.git` holds a ref for each served store ref and no other, each
    /// naming the commit `git export` of that ref gives into an empty repository.
    #[test]
    fn refresh_serves_each_ref_as_its_export() {
        let site = Site::new();
        let done = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert_eq!((done.moved, done.removed), (4, 0), "{done:?}");
        assert!(done.objects > 0, "{done:?}");

        let expected: BTreeMap<String, GitOid> = SERVED
            .iter()
            .map(|(store, git)| ((*git).to_owned(), site.exported(store)))
            .collect();
        assert_eq!(site.served(), expected);
    }

    /// A refresh with nothing new writes nothing: no ref moves, no object, no file of
    /// `serve.git` changes.
    #[test]
    fn a_second_refresh_writes_nothing() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        let before = files(site.repo.git_dir().as_std_path());

        let again = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh again");
        assert_eq!(
            (again.moved, again.removed, again.objects),
            (0, 0, 0),
            "{again:?}"
        );
        assert_eq!(files(site.repo.git_dir().as_std_path()), before);
    }

    /// A store ref moved back (`op undo`, which never takes `main` back) moves back in
    /// `serve.git` too, by the forced export; one deleted is deleted.
    #[test]
    fn refresh_follows_an_undo_and_a_deletion() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        let release_before = site.served()["refs/heads/release/1"];

        // 1. A commit on release/1, served, then undone: release/1 is served where it was.
        site.commit("d", Some("release/1"));
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert_ne!(site.served()["refs/heads/release/1"], release_before);
        let log = site.ops.op_log(OpLogReq::default()).expect("op log");
        let newest = log.ops.first().expect("an operation").id.clone();
        site.ops
            .undo(&site.who, UndoReq::new(newest))
            .expect("undo");
        let undone = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert_eq!(undone.moved, 1, "{undone:?}");
        assert_eq!(site.served()["refs/heads/release/1"], release_before);

        // 2. feature/x deleted in the store, then in serve.git.
        let head = site
            .ops
            .repo
            .get_ref("feature/x")
            .expect("read")
            .expect("there");
        let req = RefDeleteReq::new("feature/x", head.target.to_string());
        site.ops.ref_delete(&site.who, req).expect("delete");
        let deleted = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert_eq!((deleted.moved, deleted.removed), (0, 1), "{deleted:?}");
        assert!(!site.served().contains_key("refs/heads/feature/x"));
    }

    /// A store name git takes no ref for (`release/a..b`, a FORMAT 1 rule) is left out of
    /// `serve.git` with one warning; the rest is served.
    #[test]
    fn a_name_git_refuses_is_left_out_with_a_warning() {
        let site = Site::new();
        let head = site.ops.repo.get_ref("main").expect("read").expect("main");
        site.create("release/a..b", &head.target.to_string());

        let done = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        let warned: Vec<&String> = (done.warnings.iter())
            .filter(|warning| warning.contains("release/a..b"))
            .collect();
        assert_eq!(warned.len(), 1, "{:?}", done.warnings);
        let names: BTreeSet<String> = site.served().into_keys().collect();
        let expected: BTreeSet<String> = SERVED.iter().map(|(_, git)| (*git).to_owned()).collect();
        assert_eq!(names, expected);
    }

    /// A ref under a served branch's name (`feature/x/y` beside `feature/x`), which git's files
    /// cannot hold beside it, is left out with a warning.
    #[test]
    fn a_ref_under_a_served_name_is_left_out() {
        let site = Site::new();
        let head = site.ops.repo.get_ref("main").expect("read").expect("main");
        site.create("feature/x/y", &head.target.to_string());

        let done = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert!(
            done.warnings.iter().any(|w| w.contains("feature/x/y")),
            "{:?}",
            done.warnings
        );
        assert!(site.served().contains_key("refs/heads/feature/x"));
        assert!(!site.served().contains_key("refs/heads/feature/x/y"));
    }

    /// `Scope::Refs` exports the refs it names and touches no other.
    #[test]
    fn a_scoped_refresh_touches_its_refs_alone() {
        let site = Site::new();
        let scope = Scope::Refs(vec!["release/1".to_owned()]);
        let done = refresh(&site.ops, &site.repo, &scope).expect("refresh");
        assert_eq!(done.moved, 1, "{done:?}");
        let names: Vec<String> = site.served().into_keys().collect();
        assert_eq!(names, ["refs/heads/release/1"]);
    }

    /// A refresh reads the committed snapshot: while a writer holds the store's lock with a
    /// frame written and not synced, the refresh waits; the writer's sync then fails and it cuts
    /// the frame, and `main` is served at its last committed head (the M8 harness of a
    /// committed snapshot).
    #[test]
    fn refresh_serves_the_committed_head() {
        let site = Site::new();
        let committed = site.exported("main");
        let ahead = site
            .ops
            .repo
            .get_ref("feature/x")
            .expect("read")
            .expect("there");
        let main = site.ops.repo.get_ref("main").expect("read").expect("main");

        // 1. A writer moving main, held between its frame's write and its sync.
        let journal = Layout::with_format(&site.root, StoreFormat::V2).journal_log();
        let (fs, held) = HoldThenFail::new(journal, RealFs::new());
        let writer = LocalRepo::open_with_fs(&site.root, Arc::new(fs)).expect("open");
        let root: &Utf8Path = &site.root;
        thread::scope(|scope| {
            let writing = scope
                .spawn(|| writer.cas_ref("main", Some(main.target), Some(ahead.target), AGENT));
            held.wait();

            // 2. The refresh waits for the writer's lock.
            let (started, starting) = mpsc::channel();
            let refreshing = scope.spawn(move || {
                let ops = LocalOps::open(root).expect("open");
                let repo = ServeRepo::open(root).expect("open serve.git");
                started.send(()).expect("say it started");
                refresh(&ops, &repo, &Scope::All)
            });
            starting.recv().expect("the refresh started");
            thread::sleep(Duration::from_millis(300));
            assert!(
                !refreshing.is_finished(),
                "the refresh read past the held writer"
            );

            // 3. The writer's sync fails: the frame is cut, and main is where it was.
            held.release();
            match writing.join().expect("the writer") {
                Err(RepoError::Store(StoreError::NotCommitted { .. })) => {}
                other => panic!("expected NotCommitted, got {other:?}"),
            }
            refreshing.join().expect("the refresh").expect("refresh");
        });
        assert_eq!(site.served()["refs/heads/main"], committed);
    }

    /// Four refreshes at once, each its own connection, end in the same refs, and none fails.
    #[test]
    fn concurrent_refreshes_agree() {
        let site = Site::new();
        let root: &Utf8Path = &site.root;
        thread::scope(|scope| {
            let runs: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(move || {
                        let ops = LocalOps::open(root).expect("open");
                        let repo = ServeRepo::open(root).expect("open serve.git");
                        refresh(&ops, &repo, &Scope::All)
                    })
                })
                .collect();
            for run in runs {
                run.join().expect("a refresh").expect("refreshed");
            }
        });
        let expected: BTreeMap<String, GitOid> = SERVED
            .iter()
            .map(|(store, git)| ((*git).to_owned(), site.exported(store)))
            .collect();
        assert_eq!(site.served(), expected);
    }

    /// A ref of `serve.git` no store ref is served as (one put there by hand) is deleted.
    #[test]
    fn a_stray_git_ref_is_deleted() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        let main = site.served()["refs/heads/main"];
        let lock = site.repo.lock(Duration::ZERO).expect("the lock");
        site.repo
            .set_ref(&lock, "refs/notes/x", None, Some(main))
            .expect("plant a ref");
        drop(lock);

        let done = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert_eq!(done.removed, 1, "{done:?}");
        assert!(!site.served().contains_key("refs/notes/x"));
    }
}
