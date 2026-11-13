//! What `serve.git` advertises is the committed snapshot's export (M14 design §4): before each
//! advertisement, every served store ref exported into it, and a ref the store no longer has
//! removed, unless `git/serve.state` shows nothing moved since the last export, or that the last
//! full export covered everything the store held when the connection started (the M17 design
//! §4.2, the M17 closure design §3.3).
//!
//! ```text
//! freshness, lock-free: a snapshot (Repo::snapshot): its refs, s0 = its next_seq ─┐
//!                       then git/serve.state ───────────────────────────────────────┼─ Current:
//!                       serve.git's refs ───────────────────────────────────────────┘
//!   refs = the state's, and (store refs = the state's, or covered ≥ s0); else Stale, refresh:
//!
//! a s0, lock-free                       d covered ≥ the snapshot's next_seq? done (newer there)
//! b serve.lock (bounded wait, E_BUSY)     each git ref no store ref maps to: deleted by CAS
//!   git locks swept; Current? done        each served ref: export_in(force) into serve.git on
//! c serve.lock let go; one committed      that snapshot, again on a concurrent move, 8 times
//!   snapshot (the writer lock's wait);    at most; git/serve.state last, covered = next_seq
//!   serve.lock again in what is left;     then git gc --auto, serve.lock let go,
//!   serve.git or its state written        once objects were written
//!   meanwhile? c again, 8 rounds at most,
//!   then E_BUSY
//! ```
//!
//! The export is `git export`'s own ([`swarmops_git::export::export_in`]), forced: `serve.git` is
//! SwarmOps' cache, and a store ref that moved back (`op undo`) is advertised where it is. Every
//! export reads its head in one committed snapshot, so nothing a power loss or a failed sync's
//! cut could take back is advertised. A store name git takes no ref for, and one under a served
//! branch's name (`a/b` beside `a`, which git's files cannot hold both of), is left out with a
//! warning.
//!
//! `git/serve.state` (version 2) says `covered`, the `next_seq` of the committed snapshot the
//! last full refresh exported, then, for each served git ref, the changeset and the commit of its
//! last committed export; only an export under `serve.lock` writes it, after the refs it names
//! moved, and it keeps no entry `serve.git` no longer names. A push's or a land's export keeps
//! `covered` as it was. A connection whose lock-free reads find it current advertises a committed
//! export with no lock taken: every land acknowledged before it started has a seq below `s0`. A
//! file missing, cut short, of version 1 or of another first line is `Stale`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::time::{Duration, Instant, SystemTime};

use camino::Utf8PathBuf;
use swarmops_api::{ErrorCode, OpError};
use swarmops_core::{ChangesetId, ObjectId};
use swarmops_git::export::{export, export_in};
use swarmops_git::serve::refmap::to_git;
use swarmops_git::serve::repo::{ServeError, ServeLock, ServeRepo};
use swarmops_git::transport::{GitCli, Transport as _};
use swarmops_git::{
    BranchUpdate, DivergedReason, ExportError, ExportReport, ExportRequest, GitOid, GitSink as _,
    GixSink, SinkError,
};
use swarmops_repo::RepoRead;
use swarmops_store::{FsObjectStore, RefKind, TailRepair, ref_kind};

use super::not_built;
use crate::LocalOps;
use crate::error;

/// The branch `serve.git` holds with its history, which a ref new there forks from.
const MAIN: &str = "main";

/// How many times a refresh reads a ref again after its compare-and-swap in `serve.git` failed,
/// and how many rounds it takes its committed snapshot in while exports move `serve.git`.
pub const SERVE_REFRESH_ATTEMPTS: u32 = 8;

/// How long a refresh, a push's or a land's export, and a preparation wait for
/// `git/serve.lock`: the store's writer lock wait (the limits page's "Writer lock wait"), never
/// more. The one copy (the M17 design §10, D1).
pub(crate) const SERVE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// The served refs' table beside `serve.git`, in `git/`, the temp file it is written through,
/// and its first line.
const STATE_FILE: &str = "serve.state";
const STATE_TEMP: &str = "serve.state.new";
const STATE_HEADER: &str = "swarmops-serve-state 2";

/// The state's second line, before the seq its last full refresh covered.
const COVERED_KEY: &str = "covered ";

/// The `covered` of a state no full refresh wrote: it covers nothing.
const NOTHING_COVERED: u64 = 0;

/// Where a branch and a tag are in git, as the ref map (`swarmops_git::serve::refmap`) places
/// them: read here only to cut a served git ref down to the name an export takes
/// ([`short_name`]).
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

/// Whether `serve.git` advertises the store's refs as they are ([`freshness`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Freshness {
    /// The store refs, `git/serve.state` and `serve.git`'s refs agree: advertise as it is.
    Current,
    /// Something moved, or the state cannot be read: refresh first.
    Stale,
}

/// A served store ref: its name and kind.
type ServedRef = (String, RefKind);

/// A served git ref's last committed export: its changeset and its commit.
type Exported = (ChangesetId, GitOid);

/// What `git/serve.state` says, by git ref.
type State = BTreeMap<String, Exported>;

/// `git/serve.state`, read: the seq its last full refresh covered, and its refs.
#[derive(Debug, Default)]
struct StateFile {
    /// The `next_seq` of the committed snapshot the last full refresh exported: every frame
    /// below it that was ever acknowledged is in that export.
    covered: u64,
    /// The served git refs' last committed exports.
    refs: State,
}

/// What a state write says its last full refresh covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum Covered {
    /// What the state said before: [`NOTHING_COVERED`] when it was missing or of version 1.
    Keep,
    /// The `next_seq` of the committed snapshot a full refresh exported.
    Set(u64),
}

/// What [`write_state`] changes, by git ref: the export a ref names now, or `None` for one
/// removed.
pub(crate) type StateUpdates = BTreeMap<String, Option<Exported>>;

/// Whether `repo` advertises the store `ops` serves as it is, read without the store's writer
/// lock or `serve.lock`: a lock-free snapshot first, then `git/serve.state`. Current when
/// `serve.git`'s refs are the state's and either the snapshot's served store refs are the
/// state's, or the state's last full refresh covered every frame the snapshot holds (another
/// connection refreshed since the store moved). A state file that is missing, damaged or of
/// version 1, or a `serve.git` whose refs cannot be read, is [`Freshness::Stale`].
pub fn freshness(ops: &LocalOps, repo: &ServeRepo) -> Result<Freshness, OpError> {
    freshness_with(ops, repo, || {})
}

/// [`freshness`], running `between` after the snapshot and before the state is read.
fn freshness_with(
    ops: &LocalOps,
    repo: &ServeRepo,
    between: impl FnOnce(),
) -> Result<Freshness, OpError> {
    // 1. The store first: its refs, and the seq of the first frame the snapshot does not hold.
    //    Read after it, a state that covers that seq names an export at least as new.
    let read = ops.repo.snapshot().map_err(error::op)?;
    let seq = read.next_seq();
    let listed = read.list_refs().map_err(error::op)?;
    drop(read);
    between();

    // 2. The state, and serve.git's refs against its commits.
    let Some(state) = read_state(repo) else {
        return Ok(Freshness::Stale);
    };
    if !names_its_refs(repo, &state) {
        return Ok(Freshness::Stale);
    }
    if covers(&state, seq) {
        return Ok(Freshness::Current);
    }

    // 3. The served store refs, as `refresh` maps them, against the state's changesets.
    let heads: BTreeMap<String, ChangesetId> = (listed.into_iter())
        .map(|(name, ref_state)| (name, ref_state.target))
        .collect();
    let mut ignored = Refreshed::default();
    let wanted = served(heads.keys().cloned(), &Scope::All, &mut ignored);
    let store: BTreeMap<&str, ChangesetId> = (wanted.iter())
        .map(|(full, (name, _))| (full.as_str(), heads[name]))
        .collect();
    let named: BTreeMap<&str, ChangesetId> = (state.refs.iter())
        .map(|(full, (id, _))| (full.as_str(), *id))
        .collect();
    if store != named {
        return Ok(Freshness::Stale);
    }
    Ok(Freshness::Current)
}

/// Whether `serve.git`'s refs are those `state` names, commit for commit.
fn names_its_refs(repo: &ServeRepo, state: &StateFile) -> bool {
    let Ok(git) = repo.refs() else {
        return false;
    };
    let exported: BTreeMap<&str, GitOid> = (state.refs.iter())
        .map(|(full, (_, oid))| (full.as_str(), *oid))
        .collect();
    git.iter()
        .map(|(full, oid)| (full.as_str(), *oid))
        .eq(exported)
}

/// Whether `state`'s last full refresh covered every frame below `seq`. A state that says
/// [`NOTHING_COVERED`] covers nothing.
fn covers(state: &StateFile, seq: u64) -> bool {
    state.covered != NOTHING_COVERED && state.covered >= seq
}

/// Whether `repo` serves the export of a committed snapshot that holds every frame below `seq`:
/// its state covers `seq`, and `serve.git`'s refs are the state's.
fn covered(repo: &ServeRepo, seq: u64) -> bool {
    read_state(repo).is_some_and(|state| covers(&state, seq) && names_its_refs(repo, &state))
}

/// Exports `scope`'s refs of the store `ops` serves into `repo`, from one committed snapshot,
/// under `serve.lock`, which is let go while that snapshot waits for the store's writer lock.
/// With [`Scope::All`], nothing when `serve.git` is current once the lock is held, or serves a
/// snapshot at least as new as the committed one once the lock is held again (another
/// connection refreshed meanwhile). The lock is waited for `SERVE_LOCK_WAIT` (10 s) in all;
/// `E_BUSY` when exports move `serve.git` in each of [`SERVE_REFRESH_ATTEMPTS`] let-go rounds.
pub fn refresh(ops: &LocalOps, repo: &ServeRepo, scope: &Scope) -> Result<Refreshed, OpError> {
    refresh_with(ops, repo, scope, || {})
}

/// [`refresh`], running `between` after the committed snapshot, with `serve.lock` let go.
fn refresh_with(
    ops: &LocalOps,
    repo: &ServeRepo,
    scope: &Scope,
    between: impl FnOnce(),
) -> Result<Refreshed, OpError> {
    // 1. What a connection started after this one must see: the store's seq, lock-free.
    let deadline = Instant::now() + SERVE_LOCK_WAIT;
    let seq = ops.repo.snapshot().map_err(error::op)?.next_seq();

    // 2. serve.lock, then the refresh with it.
    let lock = repo.lock(SERVE_LOCK_WAIT).map_err(serve_error)?;
    let (lock, mut refreshed) = refresh_held(ops, repo, lock, scope, (seq, deadline), between)?;

    // 3. git's housekeeping, serve.lock let go.
    drop(lock);
    housekeep(repo, &mut refreshed);
    Ok(refreshed)
}

/// [`refresh`] with `lock`, which the caller holds and gets back, held: a preparation's round.
/// It is let go around the committed snapshot and around git's housekeeping, and taken again
/// each time within [`SERVE_LOCK_WAIT`].
pub(crate) fn refresh_locked(
    ops: &LocalOps,
    repo: &ServeRepo,
    lock: ServeLock,
    scope: &Scope,
) -> Result<(ServeLock, Refreshed), OpError> {
    let deadline = Instant::now() + SERVE_LOCK_WAIT;
    let seq = ops.repo.snapshot().map_err(error::op)?.next_seq();
    let (lock, mut refreshed) = refresh_held(ops, repo, lock, scope, (seq, deadline), || {})?;
    if refreshed.objects == 0 {
        return Ok((lock, refreshed));
    }

    // git's housekeeping with serve.lock let go, then the lock again for the caller's round.
    drop(lock);
    housekeep(repo, &mut refreshed);
    let lock = repo.lock(SERVE_LOCK_WAIT).map_err(serve_error)?;
    Ok((lock, refreshed))
}

/// git's own housekeeping in `repo` (`git gc --auto`), once the exports of `refreshed` wrote
/// objects, by a caller that holds no `serve.lock`: its repack grows with the history git holds,
/// and no push or connection waits for it. A failure is a warning in `refreshed`.
fn housekeep(repo: &ServeRepo, refreshed: &mut Refreshed) {
    if refreshed.objects == 0 {
        return;
    }
    let gc = GitCli::find().and_then(|git| git.gc_auto(repo.git_dir().as_std_path()));
    if let Err(e) = gc {
        let failed = error::transport_error(e, SERVE_GIT, error::Phase::Push);
        refreshed
            .warnings
            .push(format!("gc_auto_failed: {}", failed.message));
    }
}

/// Steps (b) to (d) of a refresh under `lock`: `seq` is the store's seq read before it, and
/// `deadline` ends the wait for `serve.lock` taken again; `between` runs after the committed
/// snapshot, with `serve.lock` let go.
fn refresh_held(
    ops: &LocalOps,
    repo: &ServeRepo,
    lock: ServeLock,
    scope: &Scope,
    (seq, deadline): (u64, Instant),
    between: impl FnOnce(),
) -> Result<(ServeLock, Refreshed), OpError> {
    let mut refreshed = Refreshed::default();

    // 1. What a dead process left in serve.git, cleared before any ref moves; then nothing more
    //    to do when everything is current, or covered by another connection's refresh.
    let swept = repo.sweep_git_locks(&lock).map_err(serve_error)?;
    let swept = swept
        .into_iter()
        .map(|file| format!("git_lock_removed: {file}"));
    refreshed.warnings.extend(swept);
    if *scope == Scope::All && (covered(repo, seq) || freshness(ops, repo)? == Freshness::Current) {
        return Ok((lock, refreshed));
    }

    // 2. The committed snapshot with serve.lock let go: its wait for the store's writer lock
    //    holds no other connection back. Then serve.lock again, within what is left of the wait.
    //    An export meanwhile (a push's, a land's, another refresh's) may have read a newer
    //    snapshot, which must not move serve.git back: the round again, serve.lock let go each
    //    time (I4), SERVE_REFRESH_ATTEMPTS rounds at most, then E_BUSY.
    let mut between = Some(between);
    let mut lock = lock;
    for _ in 0..SERVE_REFRESH_ATTEMPTS {
        let before = written(repo);
        drop(lock);
        let (read, recovered) = ops.repo.snapshot_committed().map_err(error::op)?;
        if let Some(between) = between.take() {
            between();
        }
        let left = deadline.saturating_duration_since(Instant::now());
        lock = repo.lock(left).map_err(serve_error)?;
        if moved(repo, &before) {
            continue;
        }

        // 3. The export of that snapshot.
        export_from(ops, repo, &lock, (&read, recovered), scope, &mut refreshed)?;
        return Ok((lock, refreshed));
    }
    Err(serve_error(ServeError::Busy))
}

/// Whether an export into `repo` wrote since `before` was read. `serve.git`'s refs unreadable
/// now is not a move: no export from them follows (`export_from` reads them first and fails).
/// Unreadable before and readable now is one.
fn moved(repo: &ServeRepo, before: &Written) -> bool {
    let now = written(repo);
    if now.1.is_none() {
        return false;
    }
    before.1.is_none() || now != *before
}

/// What an export into `repo` changes: `git/serve.state`'s bytes and modification time
/// (`None` when it cannot be read), and `serve.git`'s refs (`None` likewise).
type Written = (
    Option<(String, SystemTime)>,
    Option<BTreeMap<String, GitOid>>,
);

/// [`Written`], read now.
fn written(repo: &ServeRepo) -> Written {
    let path = state_dir(repo).join(STATE_FILE);
    let text = fs::read_to_string(&path).ok();
    let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
    let state = text.zip(modified);
    (state, repo.refs().ok())
}

/// The variable that names the phase log (test hooks).
#[cfg(feature = "test-hooks")]
const PHASE_LOG_ENV: &str = "SWARMOPS_TEST_PHASE_LOG";

/// The phase log's names of a refresh's exports under `serve.lock` (test hooks): the mark, then
/// the counts of [`swarmops_git::export::work_counts`].
#[cfg(feature = "test-hooks")]
const REFRESH_WORK: [&str; 4] = [
    "serve.refresh.held",
    "serve.refresh.exports",
    "serve.refresh.export_of",
    "serve.refresh.nodes",
];

/// What a refresh's exports do under `serve.lock`, counted for the phase log (test hooks), in
/// one write when it ends: a detached preparation's lines never split a connection's. Nothing
/// without the hooks.
///
/// ```text
///   serve.refresh.held 0 0  serve.refresh.{exports,export_of,nodes} 0 <n>
/// ```
struct Counted {
    #[cfg(feature = "test-hooks")]
    start: [u64; 3],
}

impl Counted {
    /// The counted span, from now.
    fn start() -> Counted {
        Counted {
            #[cfg(feature = "test-hooks")]
            start: swarmops_git::export::work_counts(),
        }
    }
}

#[cfg(feature = "test-hooks")]
impl Drop for Counted {
    fn drop(&mut self) {
        use std::fmt::Write as _;

        let now = swarmops_git::export::work_counts();
        let [mark, names @ ..] = REFRESH_WORK;
        let mut lines = format!("{mark} 0 0\n");
        for (name, (now, start)) in names.iter().zip(now.iter().zip(self.start)) {
            let _ = writeln!(lines, "{name} 0 {}", now - start);
        }
        append(&lines);
    }
}

/// Appends `text` to the phase log, in one write; nothing without it. A log that cannot be
/// written is passed over: a test hook does not fail the work it watches.
#[cfg(feature = "test-hooks")]
pub(super) fn append(text: &str) {
    let Some(path) = std::env::var_os(PHASE_LOG_ENV) else {
        return;
    };
    let file = fs::OpenOptions::new().create(true).append(true).open(path);
    if let Ok(mut file) = file {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Exports `scope`'s refs into `repo` from the committed snapshot `read`, under `lock`; with
/// [`Scope::All`], nothing when `serve.git` already serves one at least as new: an older
/// snapshot never moves it back.
fn export_from(
    ops: &LocalOps,
    repo: &ServeRepo,
    lock: &ServeLock,
    (read, recovered): (&RepoRead<'_, FsObjectStore>, TailRepair),
    scope: &Scope,
    refreshed: &mut Refreshed,
) -> Result<(), OpError> {
    // 1. A newer export there already: another connection's, made while this one waited. A
    //    full refresh's past this snapshot is newer even where serve.git moved past its state.
    let seq = read.next_seq();
    let past = read_state(repo).is_some_and(|state| state.covered > seq);
    if *scope == Scope::All && (past || covered(repo, seq)) {
        return Ok(());
    }

    // 2. The store's refs, in that snapshot, mapped to the git refs they are served as.
    let listed = read.list_refs().map_err(error::op)?;
    let names = listed.into_iter().map(|(name, _)| name);
    let wanted = served(names, scope, refreshed);
    let mut updates = StateUpdates::new();

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
            delete(repo, lock, &full, oid)?;
            updates.insert(full, None);
            refreshed.removed += 1;
        }
    }

    // 4. Each served ref exported on that snapshot; one the store deleted meanwhile is deleted
    //    in git too.
    let _counted = Counted::start();
    let snapshot = (read, recovered);
    for (full, (store, kind)) in &wanted {
        let Some(report) = export_one(ops, repo, Some(snapshot), full, (store, *kind))? else {
            let current = repo.refs().map_err(serve_error)?;
            if let Some(oid) = current.get(full) {
                delete(repo, lock, full, *oid)?;
                refreshed.removed += 1;
            }
            updates.insert(full.clone(), None);
            continue;
        };
        updates.insert(full.clone(), Some((report.head, report.head_oid)));
        refreshed.moved += u64::from(report.branch_moved);
        refreshed.objects += report.objects_written;
        refreshed.warnings.extend(report.warnings);
    }

    // 5. The state, last: a failure leaves the next connection a slow path, and is a warning.
    //    Only an export of every ref says what it covered.
    let said = match scope {
        Scope::All => Covered::Set(seq),
        _ => Covered::Keep,
    };
    if let Err(e) = write_state_covered(repo, lock, &updates, said) {
        refreshed
            .warnings
            .push(format!("state_not_written: {}", e.message));
    }
    Ok(())
}

/// The served store refs `scope` names among `names`, every ref the store holds, by the git ref
/// each is served as. A remote-tracking ref is passed over; a name git takes no ref for, or one
/// under a served branch's name, is left out, with a warning in `refreshed` when `scope` names
/// it. A name is under a served one whether or not `scope` names that one: the store's refs
/// decide, as in a full refresh.
fn served(
    names: impl Iterator<Item = String>,
    scope: &Scope,
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
            None if in_scope(scope, &name) => refreshed
                .warnings
                .push(format!("not_served: {name}: git takes no ref of that name")),
            None => {}
        }
    }

    // A ref whose name has a served ref's name as a directory: git's files hold one of the two.
    // In name order a directory's ref comes first, and is kept.
    let mut kept: BTreeMap<String, ServedRef> = BTreeMap::new();
    for (full, served) in mapped {
        let under = ancestors(&full).find(|dir| kept.contains_key(*dir));
        let Some(dir) = under else {
            kept.insert(full, served);
            continue;
        };
        if in_scope(scope, &served.0) {
            refreshed.warnings.push(format!(
                "not_served: {}: {dir} is served, and git holds no ref under a ref's name",
                served.0
            ));
        }
    }

    // The scope's refs among them.
    kept.retain(|_, (name, _)| in_scope(scope, name));
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

/// The name an export takes for the served git ref `full`, a branch's or a tag's: what follows
/// `refs/heads/` or `refs/tags/`. `short_name("refs/heads/release/1")` is `release/1`.
pub(super) fn short_name(full: &str) -> &str {
    full.strip_prefix(HEADS)
        .or_else(|| full.strip_prefix(TAGS))
        .unwrap_or(full)
}

/// Exports the store ref `store` of kind `kind` into `full` of `repo`, forced, by
/// compare-and-swap, under `serve.lock`, which the caller holds: on `snapshot` (a committed one
/// and its tail repair) when given, else on a committed snapshot of its own; again on a fresh
/// committed snapshot while another writer moves it, [`SERVE_REFRESH_ATTEMPTS`] times at most;
/// `None` when the store no longer has the ref. A refresh's, a push's and a land's export.
pub(super) fn export_one(
    ops: &LocalOps,
    repo: &ServeRepo,
    snapshot: Option<(&RepoRead<'_, FsObjectStore>, TailRepair)>,
    full: &str,
    (store, kind): (&str, RefKind),
) -> Result<Option<ExportReport>, OpError> {
    let short = short_name(full);
    let mut req = match kind {
        RefKind::Tag => ExportRequest::tag(store, short),
        _ => ExportRequest::new(store, short),
    };
    req.force = true;
    req.memo_key.clone_from(&ops.memo_key);
    let failed = |e: ExportError| error::git_export_error(e, SERVE_GIT, store, short);

    // Each attempt opens serve.git afresh: what gix cached of its refs is not read again. The
    // first exports on the caller's snapshot, if any; a retry on a committed snapshot of its own.
    let mut snapshot = snapshot;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut sink = GixSink::open(repo.git_dir().as_std_path())
            .map_err(|e| serve_error(ServeError::Sink(e)))?;

        // serve.git's main holds its history: a ref new there stops at its fork, not at the
        // first packed commit (a walk as long as the loose history, under serve.lock).
        let main = sink
            .branch(MAIN)
            .map_err(|e| serve_error(ServeError::Sink(e)))?;
        req.held = main.into_iter().collect();
        let exported = match snapshot.take() {
            Some((read, recovered)) => export_in(&ops.repo, read, recovered, &mut sink, &req),
            None => export(&ops.repo, &mut sink, &req),
        };
        match exported {
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

/// Writes `git/serve.state` under `lock`, after the `serve.git` refs `updates` names moved,
/// keeping what it said its last full refresh covered ([`write_state_covered`] with
/// [`Covered::Keep`]). A push's export and a land's export call it.
pub(crate) fn write_state(
    repo: &ServeRepo,
    lock: &ServeLock,
    updates: &StateUpdates,
) -> Result<(), OpError> {
    write_state_covered(repo, lock, updates, Covered::Keep)
}

/// Writes `git/serve.state` under `lock`, after the `serve.git` refs `updates` names moved: the
/// old state's entries that `serve.git` still names, then `updates` applied, kept only where
/// `serve.git` names their commit now, so that it never claims more than `serve.git` holds; and
/// the seq `covered` says. Replaced whole: a temp file in `git/` synced, renamed over it, and
/// `git/` synced. A state that cannot be read counts as empty, covering nothing.
pub(crate) fn write_state_covered(
    repo: &ServeRepo,
    lock: &ServeLock,
    updates: &StateUpdates,
    covered: Covered,
) -> Result<(), OpError> {
    let _ = lock;

    // 1. The old state and the updates, against what serve.git names now.
    let old = read_state(repo).unwrap_or_default();
    let covered = match covered {
        Covered::Keep => old.covered,
        Covered::Set(seq) => seq,
    };
    let mut state = old.refs;
    for (full, update) in updates {
        match update {
            Some(exported) => state.insert(full.clone(), *exported),
            None => state.remove(full),
        };
    }
    let names = repo.refs().map_err(serve_error)?;
    state.retain(|full, (_, oid)| names.get(full) == Some(oid));

    // 2. The bytes: the header, what it covered, then a line per ref in name order.
    let lines = (state.iter()).map(|(full, (id, oid))| format!("{full} {id} {oid}\n"));
    let head = format!("{STATE_HEADER}\n{COVERED_KEY}{covered}\n");
    let text: String = std::iter::once(head).chain(lines).collect();

    // 3. Replaced whole, durably.
    let dir = state_dir(repo);
    let failed = |what: &str| {
        let what = what.to_owned();
        move |e: io::Error| {
            serve_error(ServeError::Io {
                what: what.clone(),
                reason: e.kind().to_string(),
            })
        }
    };
    let temp = dir.join(STATE_TEMP);
    let mut file = File::create(temp.as_std_path()).map_err(failed(STATE_TEMP))?;
    file.write_all(text.as_bytes())
        .map_err(failed(STATE_TEMP))?;
    file.sync_data().map_err(failed(STATE_TEMP))?;
    drop(file);
    let path = dir.join(STATE_FILE);
    fs::rename(temp.as_std_path(), path.as_std_path()).map_err(failed(STATE_FILE))?;
    File::open(dir.as_std_path())
        .and_then(|dir| dir.sync_all())
        .map_err(failed(""))
}

/// What `git/serve.state` says; `None` when it is missing, cannot be read, has another first
/// line (version 1's among them), no `covered <seq>` second line, or a line that is no
/// `<git ref> <changeset> <commit>` in name order.
fn read_state(repo: &ServeRepo) -> Option<StateFile> {
    let text = fs::read_to_string(state_dir(repo).join(STATE_FILE)).ok()?;
    let body = text.strip_prefix(STATE_HEADER)?.strip_prefix('\n')?;
    let (covered, body) = body.strip_prefix(COVERED_KEY)?.split_once('\n')?;
    if covered.is_empty() || !covered.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let covered = covered.parse().ok()?;
    if !(body.is_empty() || body.ends_with('\n')) {
        return None;
    }

    // Each line whole, in strictly rising name order.
    let mut refs = State::new();
    for line in body.lines() {
        let mut fields = line.split(' ');
        let (Some(full), Some(id), Some(oid), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        let id = ChangesetId::new(ObjectId::from_hex(id).ok()?);
        let oid = GitOid::from_hex(oid.as_bytes())?;
        if refs
            .last_key_value()
            .is_some_and(|(last, _)| last.as_str() >= full)
        {
            return None;
        }
        refs.insert(full.to_owned(), (id, oid));
    }
    Some(StateFile { covered, refs })
}

/// `git/`, which holds `serve.git` and the state.
fn state_dir(repo: &ServeRepo) -> Utf8PathBuf {
    let git_dir = repo.git_dir();
    git_dir.parent().unwrap_or(git_dir).to_owned()
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
    use swarmops_git::{BranchUpdate, ExportRequest, GitOid, GixSink, export};
    use swarmops_repo::{LocalRepo, RepoError, StoreError};
    use swarmops_store::{Layout, RealFs, StoreFormat};
    use swarmops_testkit::fs::HoldThenFail;
    use tempfile::TempDir;

    use super::{
        Covered, Freshness, Refreshed, Scope, StateUpdates, export_from, freshness, freshness_with,
        moved, refresh, refresh_with, write_state_covered, written,
    };
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

    /// `git/serve.state` under the store's root.
    const STATE: &str = "git/serve.state";

    /// How long a test waits for what another thread does: far past it, for a loaded host.
    const WITHIN: Duration = Duration::from_secs(5);

    /// How often a test looks again while it waits.
    const POLL: Duration = Duration::from_millis(10);

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

        /// `git/serve.state`'s text.
        fn state(&self) -> String {
            std::fs::read_to_string(self.root.join(STATE)).expect("read serve.state")
        }

        /// Whether `serve.git` is current, read as a connection reads it.
        fn current(&self) -> bool {
            freshness(&self.ops, &self.repo).expect("freshness") == Freshness::Current
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

    /// `Scope::Refs` naming a ref under a served branch's name (`feature/x/y` beside
    /// `feature/x`) leaves it out with the full refresh's warning: no error, and nothing moves.
    #[test]
    fn a_scoped_ref_under_a_served_name_is_left_out() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        let head = site.ops.repo.get_ref("main").expect("read").expect("main");
        site.create("feature/x/y", &head.target.to_string());
        let served = site.served();

        let scope = Scope::Refs(vec!["feature/x/y".to_owned()]);
        let done = refresh(&site.ops, &site.repo, &scope).expect("a warning, not an error");
        let warning = "not_served: feature/x/y: refs/heads/feature/x is served, and git holds no \
                       ref under a ref's name";
        assert_eq!(done.warnings, [warning]);
        assert_eq!((done.moved, done.removed), (0, 0), "{done:?}");
        assert_eq!(site.served(), served);
    }

    /// A scoped refresh warns of the refs it names alone: a name git takes no ref for, outside
    /// its scope, is the full refresh's to report.
    #[test]
    fn a_scoped_refresh_warns_of_its_refs_alone() {
        let site = Site::new();
        let head = site.ops.repo.get_ref("main").expect("read").expect("main");
        site.create("feature/x/y", &head.target.to_string());

        let scope = Scope::Refs(vec!["release/1".to_owned()]);
        let done = refresh(&site.ops, &site.repo, &scope).expect("refresh");
        assert_eq!(done.warnings, [] as [&str; 0]);
    }

    /// An export that writes `git/serve.state` alone, `serve.git`'s refs as they were, is a
    /// move: the state half of what `moved` compares.
    #[test]
    fn a_state_written_alone_is_a_move() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        let before = written(&site.repo);
        assert!(!moved(&site.repo, &before), "nothing wrote");

        let lock = site.repo.lock(Duration::ZERO).expect("serve.lock");
        let covered = Covered::Set(covered_of(&site.state()) + 1);
        write_state_covered(&site.repo, &lock, &StateUpdates::new(), covered).expect("the state");
        drop(lock);
        assert_eq!(written(&site.repo).1, before.1, "the refs as they were");
        assert!(moved(&site.repo, &before));
    }

    /// An export that moves a ref of `serve.git` and writes no state is a move: the refs half of
    /// what `moved` compares.
    #[test]
    fn a_ref_moved_alone_is_a_move() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        let before = written(&site.repo);
        let served = site.served();
        let (release, main) = (served["refs/heads/release/1"], served["refs/heads/main"]);

        let lock = site.repo.lock(Duration::ZERO).expect("serve.lock");
        let update = site
            .repo
            .set_ref(&lock, "refs/heads/release/1", Some(release), Some(main));
        assert!(matches!(update, Ok(BranchUpdate::Applied)), "{update:?}");
        drop(lock);
        assert_eq!(written(&site.repo).0, before.0, "the state as it was");
        assert!(moved(&site.repo, &before));
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

    /// A refresh reads its refs and their heads in one committed snapshot: four served refs, one
    /// of them moved, take one `read_committed`.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn a_refresh_takes_one_committed_snapshot() {
        use swarmops_store::MetaStore;

        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        site.commit("d", Some("release/1"));

        let before = MetaStore::read_committed_calls();
        let done = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert_eq!(done.moved, 1, "{done:?}");
        assert_eq!(MetaStore::read_committed_calls() - before, 1);
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

    /// A state of version 1 (no `covered` line) whose refs match is not current: its lack of
    /// `covered` costs a slow path, never a wrong advertisement.
    #[test]
    fn a_version_1_state_is_not_current() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert!(site.current());

        // The file as an older binary writes it: its header, then the refs alone.
        let refs = site.state();
        let refs = refs
            .lines()
            .skip(1)
            .filter(|line| !line.starts_with("covered "));
        let lines: Vec<&str> = std::iter::once("swarmops-serve-state 1")
            .chain(refs)
            .collect();
        let text = lines.join("\n") + "\n";
        std::fs::write(site.root.join(STATE), text).expect("write serve.state");
        assert!(!site.current());
    }

    /// A land, then a refresh that covers it: a connection started after the land finds
    /// `serve.git` current, takes no committed snapshot, and advertises the land.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn a_land_before_a_covered_fetch_is_advertised() {
        use swarmops_store::MetaStore;

        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        site.commit("d", Some("main"));
        refresh(&site.ops, &site.repo, &Scope::All).expect("the refresh that covers it");
        let landed = site.exported("main");

        let before = MetaStore::read_committed_calls();
        assert!(site.current());
        let done = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert_eq!(MetaStore::read_committed_calls() - before, 0);
        assert_eq!((done.moved, done.objects), (0, 0), "{done:?}");
        assert_eq!(site.served()["refs/heads/main"], landed);
    }

    /// A land after the last refresh is not hidden by it: the connection finds `serve.git`
    /// stale, and its refresh serves the land.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn a_land_after_covered_is_not_hidden() {
        use swarmops_store::MetaStore;

        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        site.commit("d", Some("main"));

        assert!(!site.current());
        let before = MetaStore::read_committed_calls();
        let done = refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        assert_eq!(MetaStore::read_committed_calls() - before, 1);
        assert_eq!(done.moved, 1, "{done:?}");
        assert_eq!(site.served()["refs/heads/main"], site.exported("main"));
    }

    /// `freshness` reads the store before the state: a land and a refresh that covers it,
    /// both between the two reads, leave the connection current, with the land advertised.
    #[test]
    fn a_land_between_the_reads_is_covered() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");

        let between = || {
            site.commit("d", Some("main"));
            refresh(&site.ops, &site.repo, &Scope::All).expect("the refresh between the reads");
        };
        let fresh = freshness_with(&site.ops, &site.repo, between).expect("freshness");
        assert_eq!(fresh, Freshness::Current);
        assert_eq!(site.served()["refs/heads/main"], site.exported("main"));
    }

    /// A refresh waiting for the store's writer lock holds no `serve.lock`: another connection
    /// takes it meanwhile; once the writer lets go, the refresh ends and serves every ref.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn serve_lock_is_free_during_the_writer_wait() {
        use std::time::Instant;
        use swarmops_store::{MetaLock, MetaStore};

        let site = Site::new();
        let layout = Layout::with_format(&site.root, StoreFormat::V2);
        let root: &Utf8Path = &site.root;
        let before = MetaStore::read_committed_calls();
        let writer = MetaLock::acquire(&layout, WITHIN).expect("the writer lock");
        thread::scope(|scope| {
            let refreshing = scope.spawn(move || {
                let ops = LocalOps::open(root).expect("open");
                let repo = ServeRepo::open(root).expect("open serve.git");
                refresh(&ops, &repo, &Scope::All)
            });

            // 1. The refresh waits for the writer lock.
            let deadline = Instant::now() + WITHIN;
            while MetaStore::read_committed_calls() == before {
                assert!(
                    Instant::now() < deadline,
                    "the refresh never read the store"
                );
                thread::sleep(POLL);
            }

            // 2. serve.lock is free meanwhile.
            let deadline = Instant::now() + WITHIN;
            let taken = loop {
                if let Ok(lock) = site.repo.lock(Duration::ZERO) {
                    break lock;
                }
                assert!(
                    Instant::now() < deadline,
                    "serve.lock held across the writer wait"
                );
                thread::sleep(POLL);
            };
            assert!(!refreshing.is_finished());
            drop(taken);

            // 3. The writer lets go: the refresh ends.
            drop(writer);
            refreshing.join().expect("the refresh").expect("refresh");
        });
        let expected: BTreeMap<String, GitOid> = SERVED
            .iter()
            .map(|(store, git)| ((*git).to_owned(), site.exported(store)))
            .collect();
        assert_eq!(site.served(), expected);
    }

    /// A land's export in the let-go window moves `serve.git` past the refresh's snapshot, and a
    /// writer then holds the store's lock: the refresh's next snapshot waits for it with
    /// `serve.lock` let go (I4 on the moved path), and it serves the land, never older.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn refresh_never_exports_older_than_it_read_when_main_moves() {
        use std::sync::Mutex;
        use std::time::Instant;
        use swarmops_store::{MetaLock, MetaStore};

        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        site.commit("d", Some("main"));
        let layout = Layout::with_format(&site.root, StoreFormat::V2);
        let writer: Mutex<Option<MetaLock>> = Mutex::new(None);
        let (said, told) = mpsc::channel();

        // 1. The land in the let-go window: commit e, its export under serve.lock; then the
        //    writer lock, held by the test.
        let between = || {
            site.commit("e", Some("main"));
            let lock = site.repo.lock(Duration::ZERO).expect("serve.lock");
            let (fresh, repaired) = site.ops.repo.snapshot_committed().expect("a snapshot");
            let scope = Scope::Refs(vec!["main".to_owned()]);
            let mut done = Refreshed::default();
            export_from(
                &site.ops,
                &site.repo,
                &lock,
                (&fresh, repaired),
                &scope,
                &mut done,
            )
            .expect("the land's export");
            drop((fresh, lock));
            let held = MetaLock::acquire(&layout, WITHIN).expect("the writer lock");
            *writer.lock().expect("the writer slot") = Some(held);
            said.send(MetaStore::read_committed_calls())
                .expect("say the writer holds");
        };
        thread::scope(|scope| {
            let refreshing =
                scope.spawn(|| refresh_with(&site.ops, &site.repo, &Scope::All, between));

            // 2. The refresh's next snapshot waits for the writer.
            let before = told.recv().expect("the writer holds");
            let deadline = Instant::now() + WITHIN;
            while MetaStore::read_committed_calls() == before {
                assert!(
                    Instant::now() < deadline,
                    "the refresh never read the store again"
                );
                thread::sleep(POLL);
            }

            // 3. serve.lock is free meanwhile.
            let deadline = Instant::now() + WITHIN;
            let taken = loop {
                if let Ok(lock) = site.repo.lock(Duration::ZERO) {
                    break lock;
                }
                assert!(
                    Instant::now() < deadline,
                    "serve.lock held across the writer wait on the moved path"
                );
                thread::sleep(POLL);
            };
            drop(taken);

            // 4. The writer lets go: the refresh ends.
            drop(writer.lock().expect("the writer slot").take());
            refreshing.join().expect("the refresh").expect("refresh");
        });
        assert_eq!(site.served()["refs/heads/main"], site.exported("main"));
        assert!(site.current());
    }

    /// An export from a snapshot older than the one `serve.git` serves writes nothing: no ref
    /// moves back, no object, the state as it was.
    #[test]
    fn an_older_refresh_writes_nothing() {
        let site = Site::new();
        let (older, repaired) = site.ops.repo.snapshot_committed().expect("a snapshot");
        site.commit("d", Some("main"));
        refresh(&site.ops, &site.repo, &Scope::All).expect("the newer refresh");
        let files_before = files(site.repo.git_dir().as_std_path());
        let (served, state) = (site.served(), site.state());

        let lock = site.repo.lock(Duration::ZERO).expect("serve.lock");
        let mut done = Refreshed::default();
        let snapshot = (&older, repaired);
        export_from(
            &site.ops,
            &site.repo,
            &lock,
            snapshot,
            &Scope::All,
            &mut done,
        )
        .expect("the older export");
        drop(lock);
        assert_eq!(
            (done.moved, done.removed, done.objects),
            (0, 0, 0),
            "{done:?}"
        );
        assert_eq!(site.served(), served);
        assert_eq!(site.state(), state);
        assert_eq!(files(site.repo.git_dir().as_std_path()), files_before);
    }

    /// The `covered` line of `git/serve.state`.
    fn covered_of(state: &str) -> u64 {
        let line = state.lines().nth(1).expect("a covered line");
        let seq = line.strip_prefix("covered ").expect("covered");
        seq.parse().expect("a seq")
    }

    /// A land's export of `main` made while a refresh waits with `serve.lock` let go, after its
    /// snapshot: the refresh, from that older snapshot, never moves `main` back.
    #[test]
    fn a_land_in_the_let_go_is_kept() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        site.commit("d", Some("main"));

        // The land: commit e, then its export under serve.lock (a Covered::Keep write).
        let between = || {
            site.commit("e", Some("main"));
            let lock = site.repo.lock(Duration::ZERO).expect("serve.lock");
            let (fresh, repaired) = site.ops.repo.snapshot_committed().expect("a snapshot");
            let scope = Scope::Refs(vec!["main".to_owned()]);
            let mut done = Refreshed::default();
            export_from(
                &site.ops,
                &site.repo,
                &lock,
                (&fresh, repaired),
                &scope,
                &mut done,
            )
            .expect("the land's export");
        };
        let done = refresh_with(&site.ops, &site.repo, &Scope::All, between).expect("refresh");
        assert_eq!(
            site.served()["refs/heads/main"],
            site.exported("main"),
            "{done:?}"
        );
        assert!(site.current());
    }

    /// A ref a push made while a refresh waits with `serve.lock` let go, after its snapshot:
    /// the refresh, from that older snapshot, never deletes it.
    #[test]
    fn a_push_in_the_let_go_is_kept() {
        let site = Site::new();
        refresh(&site.ops, &site.repo, &Scope::All).expect("refresh");
        site.commit("d", Some("main"));

        // The push: topic made, then its export under serve.lock (a Covered::Keep write).
        let between = || {
            let head = site.ops.repo.get_ref("main").expect("read").expect("main");
            site.create("topic", &head.target.to_string());
            let lock = site.repo.lock(Duration::ZERO).expect("serve.lock");
            let (fresh, repaired) = site.ops.repo.snapshot_committed().expect("a snapshot");
            let scope = Scope::Refs(vec!["topic".to_owned()]);
            let mut done = Refreshed::default();
            export_from(
                &site.ops,
                &site.repo,
                &lock,
                (&fresh, repaired),
                &scope,
                &mut done,
            )
            .expect("the push's export");
        };
        let done = refresh_with(&site.ops, &site.repo, &Scope::All, between).expect("refresh");
        assert_eq!(done.removed, 0, "{done:?}");
        assert_eq!(site.served()["refs/heads/topic"], site.exported("topic"));
        assert!(site.current());
    }

    /// An export from a snapshot older than the last full refresh's, with `serve.git` no longer
    /// as its state says (a state write that failed), never lowers `covered` nor moves a ref
    /// back.
    #[test]
    fn covered_never_goes_down() {
        let site = Site::new();
        let (older, repaired) = site.ops.repo.snapshot_committed().expect("a snapshot");
        site.commit("d", Some("main"));
        refresh(&site.ops, &site.repo, &Scope::All).expect("the newer refresh");
        let was = covered_of(&site.state());

        // serve.git's main moved past its state: the state names it no more.
        let lock = site.repo.lock(Duration::ZERO).expect("serve.lock");
        let main = site.served()["refs/heads/main"];
        let other = site.served()["refs/heads/feature/x"];
        site.repo
            .set_ref(&lock, "refs/heads/main", Some(main), Some(other))
            .expect("move main");

        let mut done = Refreshed::default();
        export_from(
            &site.ops,
            &site.repo,
            &lock,
            (&older, repaired),
            &Scope::All,
            &mut done,
        )
        .expect("the older export");
        drop(lock);
        assert!(covered_of(&site.state()) >= was, "{done:?}");
        assert_eq!(site.served()["refs/heads/main"], other, "{done:?}");
    }
}
