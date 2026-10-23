//! The git remotes of a store (M13): `remotes.toml`, `remote add`, `remote remove`,
//! `remote list`, `fetch` and `clone` (operations spec §1's `clone_repo`, `remote_*` and
//! `fetch` rows, §6).
//!
//! Client-side operations, as `git import` is: each spawns the user's git on this host against
//! the store's mirror, so none is a [`StoreOps`] operation; the MCP
//! server offers `remote_list` and `fetch`, which take a remote by its name only. The commands
//! that change the remotes or the mirror hold the mirror's lock for their whole run: the public
//! forms take it, and the locked forms (`*_locked`) receive it from a command that holds it
//! already (`clone`, and `sync`'s fetch), since it is not re-entrant.
//!
//! ```text
//!   fetch <remote>
//!     remotes.toml ──▶ the remote (E_REMOTE_NOT_FOUND)
//!     git/lock     ──▶ held to the end (E_BUSY at once)
//!     mirror.git   ──▶ made or set aside and made again; stale git lock files removed
//!     git fetch    ──▶ refs/remotes/<remote>/*, refs/remote-tags/<remote>/*
//!     import       ──▶ the followed branch's commits become changesets; no ref moves
//!     git gc --auto
//!   clone <url> --into <dir>
//!     init (or open, to resume) ──▶ lock ──▶ remote add ──▶ fetch ──▶ import into main
//! ```
//!
//! `remotes.toml` is read and written whole, through the store's own filesystem seam
//! ([`swarmops_repo::Repo::store_fs`]), and only under the mirror's lock when written, so two
//! writers never lose each other's line. A key this build does not know is kept when read and
//! dropped on the next write; a `version` other than 1 is `E_STORE_CORRUPT`.

use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use swarmops_api::remote::MAX_FETCH_BRANCHES;
use swarmops_api::{
    BranchUpdate, CloneReq, CloneResp, ErrorCode, FetchReq, FetchResp, Identity, InfoReq,
    LossyClass, OpError, RemoteAddReq, RemoteHead, RemoteInfo, RemoteList, RemoteListReq,
    RemoteRemoveReq, RemoteRemoveResp, StoreOps,
};
use swarmops_core::ChangesetId;
use swarmops_git::mirror::{Mirror, MirrorLock, Rebuilt};
use swarmops_git::transport::{Credential, GitCli, RefSpec, RemoteUrl, Transport};
use swarmops_git::{GitOid, GixSource, ImportError, ImportReport, ImportRequest, LossySet};
use swarmops_repo::RepoError;
use swarmops_store::{IoFailure, SYSTEM_GIT, StoreError};

use crate::error::{self, Phase};
use crate::local::{LocalOps, Recording, Resolved};

/// The store's file of remotes, in its root; the `store_file` its errors name.
const REMOTES_TOML: &str = "remotes.toml";

/// The only `version` of `remotes.toml` this build reads and writes.
const VERSION: u32 = 1;

/// The first line of every `remotes.toml` written.
const HEADER: &str = "# Written by `so remote add|remove` and `so clone`; read by fetch, push, \
                      sync and remote list.\n";

/// The mirror as errors name it: relative to the store's root, never a host path.
const MIRROR_GIT: &str = "git/mirror.git";

/// The store ref a clone imports the followed branch into.
const MAIN: &str = "main";

/// The branch a remote follows when its `HEAD` names none, and the warning that says so.
const FALLBACK_BRANCH: &str = "main";
const BRANCH_UNKNOWN: &str = "default_branch_unknown";

/// A name `so sync status` takes as its subcommand, so no remote may have it.
const SUBCOMMAND: &str = "status";

/// The longest remote name: one character then up to 62 more.
const MAX_NAME_LEN: usize = 63;

/// The longest branch or variable name accepted, in bytes.
const MAX_LEN: usize = 255;

/// The suffix git keeps for its lock files, which no branch name may end with.
const LOCK_SUFFIX: &str = ".lock";

/// `remotes.toml` as written: its version, then one table per remote.
#[derive(Serialize, Deserialize, Debug, Default)]
struct RemotesFile {
    version: u32,
    #[serde(default, rename = "remote")]
    remotes: Vec<Entry>,
}

/// One `[[remote]]` table.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Entry {
    name: String,
    url: String,
    branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_env: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lossy: Vec<LossyClass>,
}

/// The `version` key alone, read before the rest so that another version is named as such.
#[derive(Deserialize)]
struct Versioned {
    version: i64,
}

impl Entry {
    /// The remote as the API gives it, with `warnings`.
    fn info(&self, warnings: Option<Vec<String>>) -> RemoteInfo {
        let mut out = RemoteInfo::new(
            self.name.clone(),
            self.url.clone(),
            self.branch.clone(),
            self.lossy.clone(),
        );
        out.token_env.clone_from(&self.token_env);
        out.warnings = warnings;
        out
    }

    /// The entry that records `info`.
    fn of(info: &RemoteInfo) -> Entry {
        Entry {
            name: info.name.clone(),
            url: info.url.clone(),
            branch: info.branch.clone(),
            token_env: info.token_env.clone(),
            lossy: info.lossy.clone(),
        }
    }
}

/// What a clone's directory holds before the clone.
enum Place {
    /// Nothing, or an empty directory: a store is made there.
    Free,
    /// Something else: a store a clone may resume into, or not.
    Taken,
}

impl LocalOps {
    /// `clone`: a store made at `req.path` (or, to resume an interrupted clone, the one there),
    /// `req.url` recorded as `req.remote`, fetched, and its followed branch imported into
    /// `main` by `who`; the ref move is recorded in `resolved`. An associated function, as
    /// [`LocalOps::init`] is: there is no store to open before it. It needs an identity: it
    /// moves `main`.
    ///
    /// A store there is resumed when it records `req.remote` at the same URL, or records no
    /// remote and no ref (a clone stopped before it recorded its remote leaves one); anything
    /// else there is `E_EXISTS`. An error after the store was made says so
    /// (`details.created: true`), and the same clone run again resumes.
    pub fn clone_repo(
        req: CloneReq,
        who: &Identity,
        resolved: &Resolved,
    ) -> Result<(LocalOps, CloneResp), OpError> {
        let CloneReq {
            url,
            path,
            name,
            branch,
            remote,
            token_env,
            lossy,
            ..
        } = req;
        let at = Utf8Path::new(&path);

        // 1. Every argument checked before anything is made.
        RemoteUrl::parse(&url).map_err(error::url_refused)?;
        check_name(&remote)?;
        check_token_env(token_env.as_deref())?;
        if let Some(branch) = &branch {
            check_branch(branch)?;
        }
        let mut add = RemoteAddReq::new(remote, url);
        add.branch = branch;
        add.token_env = token_env;
        add.lossy = lossy;

        // 2. The store: made, or the one an interrupted clone left, opened.
        let (ops, made) = match place(at)? {
            Place::Free => {
                let store_name = name.unwrap_or_else(|| repo_name(&add.url));
                (made_store(at, &store_name)?, true)
            }
            Place::Taken => (resumable(at, &add)?, false),
        };

        // 3. Under one mirror lock: the remote, its fetch, and main.
        let resp = ops
            .recording(resolved)
            .clone_into(add, who)
            .map_err(|e| if made { created(at, e) } else { e })?;
        Ok((ops, resp))
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
    pub fn remote_add(&self, req: RemoteAddReq) -> Result<RemoteInfo, OpError> {
        let mirror = self.mirror()?;
        let lock = mirror.lock().map_err(error::mirror_error)?;
        self.remote_add_locked(&lock, req)
    }

    /// `remote remove`: forgets `req.name`, and the mirror's refs of it, under the mirror's
    /// lock, which it takes. It needs no identity: it writes remotes.toml and the mirror, no
    /// store object.
    pub fn remote_remove(&self, req: RemoteRemoveReq) -> Result<RemoteRemoveResp, OpError> {
        let RemoteRemoveReq { name, .. } = req;
        let mirror = self.mirror()?;
        let lock = mirror.lock().map_err(error::mirror_error)?;

        // 1. The remote, which must be there.
        let mut file = self.read_remotes()?;
        let Some(at) = file.remotes.iter().position(|entry| entry.name == name) else {
            return Err(error::remote_not_found(&name));
        };

        // 2. Its mirror refs first: a crash before the file's write leaves the remote, which
        // the next fetch fills again.
        let (mirror, _) = ready(mirror, &lock)?;
        let removed_refs = mirror.remove_remote(&name).map_err(error::mirror_error)?;

        // 3. Its line.
        file.remotes.remove(at);
        self.write_remotes(&file)?;
        Ok(RemoteRemoveResp::new(name, removed_refs))
    }

    /// `remote list`: the remotes `remotes.toml` records, read without the lock.
    pub fn remote_list(&self, _req: RemoteListReq) -> Result<RemoteList, OpError> {
        let file = self.read_remotes()?;
        let remotes = file.remotes.iter().map(|entry| entry.info(None)).collect();
        Ok(RemoteList::new(remotes))
    }

    /// `fetch`: the remote's branches and tags into the mirror, and its followed branch's
    /// commits into the store, under the mirror's lock, which it takes. It needs no identity:
    /// it moves no ref and records no agent.
    pub fn fetch(&self, req: FetchReq) -> Result<FetchResp, OpError> {
        let FetchReq { remote, .. } = req;
        let remote = self.remote(&remote)?;
        let mirror = self.mirror()?;
        let lock = mirror.lock().map_err(error::mirror_error)?;
        self.fetch_locked(&lock, &remote)
    }

    /// `remote add` under the mirror's `lock`, which the caller holds.
    pub(crate) fn remote_add_locked(
        &self,
        lock: &MirrorLock,
        req: RemoteAddReq,
    ) -> Result<RemoteInfo, OpError> {
        let RemoteAddReq {
            name,
            url,
            branch,
            token_env,
            lossy,
            ..
        } = req;

        // 1. The arguments, then the name's uniqueness.
        check_name(&name)?;
        let parsed = RemoteUrl::parse(&url).map_err(error::url_refused)?;
        check_token_env(token_env.as_deref())?;
        if let Some(branch) = &branch {
            check_branch(branch)?;
        }
        let mut file = self.read_remotes()?;
        if file.remotes.iter().any(|entry| entry.name == name) {
            return Err(remote_exists(&name));
        }

        // 2. The mirror, made first; then the followed branch, the remote's default when none
        // is given (a network call).
        let (_, mut warnings) = ready(self.mirror()?, lock)?;
        let branch = if let Some(branch) = branch {
            branch
        } else {
            let mut cred = Credential::default();
            cred.token_env.clone_from(&token_env);
            let (branch, unknown) = default_branch(&parsed, &cred, &name)?;
            warnings.extend(unknown);
            branch
        };

        // 3. Recorded.
        let mut info = RemoteInfo::new(name, url, branch, lossy);
        info.token_env = token_env;
        info.warnings = (!warnings.is_empty()).then_some(warnings);
        file.remotes.push(Entry::of(&info));
        self.write_remotes(&file)?;
        Ok(info)
    }

    /// `fetch` of `remote` under the mirror's `lock`, which the caller holds.
    pub(crate) fn fetch_locked(
        &self,
        lock: &MirrorLock,
        remote: &RemoteInfo,
    ) -> Result<FetchResp, OpError> {
        let name = remote.name.as_str();
        let failed = |e| error::transport_error(e, name, Phase::Fetch);

        // 1. The mirror, made or rebuilt, its stale git lock files removed.
        let (mirror, mut warnings) = ready(self.mirror()?, lock)?;
        let url = RemoteUrl::parse(&remote.url).map_err(error::url_refused)?;
        let mut cred = Credential::default();
        cred.token_env.clone_from(&remote.token_env);
        let git = GitCli::find().map_err(failed)?;

        // 2. git fetch, between two readings of the remote's refs in the mirror.
        let branches = mirror.branches(name).map_err(error::mirror_error)?;
        let tags = mirror.tags(name).map_err(error::mirror_error)?;
        git.fetch(mirror.git_dir(), &url, &cred, &refspecs(name))
            .map_err(failed)?;
        let branches_now = mirror.branches(name).map_err(error::mirror_error)?;
        let tags_now = mirror.tags(name).map_err(error::mirror_error)?;

        // 3. The followed branch's commits into the store, no ref moved.
        let old = branches.get(&remote.branch).copied();
        let new = branches_now.get(&remote.branch).copied();
        let report = match new {
            Some(_) => {
                let git_ref = tracking_ref(name, &remote.branch);
                let request = ImportRequest::objects_only(git_ref.as_str());
                Some(self.import_from(&mirror, request, &remote.lossy, &git_ref)?)
            }
            None => None,
        };

        // 4. git's housekeeping; its failure costs disk, not the fetch.
        if let Err(e) = git.gc_auto(mirror.git_dir()) {
            warnings.push(format!("gc_auto_failed: {}", failed(e).message));
        }

        // 5. The response.
        let head = report
            .as_ref()
            .map(|report| remote_head(report.git_head, report.head));
        let old_head = match (old, &head) {
            (Some(oid), Some(head)) if oid.to_string() == head.sha => Some(head.clone()),
            (Some(oid), _) => self.known_head(oid)?,
            (None, _) => None,
        };
        let imported = report.as_ref().map_or(0, |report| report.imported);
        if let Some(report) = report {
            warnings.extend(report.warnings);
        }
        let (moved, branches_truncated) = moved_branches(&branches, &branches_now);
        let mut out = FetchResp::new(
            name.to_owned(),
            remote.branch.clone(),
            imported,
            moved,
            branches_truncated,
            changed(&tags, &tags_now),
        );
        out.head = head;
        out.old_head = old_head;
        out.warnings = (!warnings.is_empty()).then_some(warnings);
        Ok(out)
    }

    /// A clone's steps on its store, under one mirror lock: `add`'s remote recorded (unless a
    /// clone it resumes recorded it), fetched, and its followed branch imported into `main` by
    /// `who`.
    fn clone_into(&self, add: RemoteAddReq, who: &Identity) -> Result<CloneResp, OpError> {
        let mirror = self.mirror()?;
        let lock = mirror.lock().map_err(error::mirror_error)?;

        // 1. The remote, and its fetch.
        let recorded = self.find(&add.name)?;
        let remote = match recorded {
            Some(entry) => entry.info(None),
            None => self.remote_add_locked(&lock, add)?,
        };
        let fetched = self.fetch_locked(&lock, &remote)?;

        // 2. The followed branch into main, as `git import` moves it; an empty remote leaves
        // no main.
        let mut imported = fetched.imported;
        let mut main = None;
        if fetched.head.is_some() {
            let git_ref = tracking_ref(&remote.name, &remote.branch);
            let request = ImportRequest::new(git_ref.as_str(), MAIN, who.agent());
            let report = self.import_from(&mirror, request, &remote.lossy, &git_ref)?;
            self.resolved.record(MAIN, report.head);
            imported += report.imported;
            main = Some(report.head.to_string());
        }

        // 3. The response.
        let repo = self.repo_info(InfoReq::default())?;
        let mut out = CloneResp::new(repo, remote, fetched, imported);
        out.main = main;
        Ok(out)
    }

    /// Imports `git_ref` of `mirror` as `request` says, with the classes `lossy` allows; the
    /// repairs its writes made are recorded.
    fn import_from(
        &self,
        mirror: &Mirror,
        mut request: ImportRequest,
        lossy: &[LossyClass],
        git_ref: &str,
    ) -> Result<ImportReport, OpError> {
        let failed = |e: ImportError| error::git_import_error(e, MIRROR_GIT, git_ref);
        let source = GixSource::open(mirror.git_dir()).map_err(|e| failed(e.into()))?;
        request.lossy = lossy_set(lossy);
        let report = swarmops_git::import(&self.ops.repo, &source, &request).map_err(failed)?;
        self.resolved.record_recovered(report.recovered);
        Ok(report)
    }

    /// The head `oid` as a fetch reports it, if the store knows its changeset.
    fn known_head(&self, oid: GitOid) -> Result<Option<RemoteHead>, OpError> {
        let entry = self
            .ops
            .repo
            .external_id(SYSTEM_GIT, &oid.0)
            .map_err(error::op)?;
        Ok(entry.map(|entry| remote_head(oid, entry.changeset)))
    }

    /// The store's mirror: `git/` and its lock made, `mirror.git` not yet.
    fn mirror(&self) -> Result<Mirror, OpError> {
        Mirror::open(self.ops.store_path()).map_err(error::mirror_error)
    }

    /// The remote named `name`, or `E_REMOTE_NOT_FOUND`.
    fn remote(&self, name: &str) -> Result<RemoteInfo, OpError> {
        let found = self.find(name)?;
        found
            .map(|entry| entry.info(None))
            .ok_or_else(|| error::remote_not_found(name))
    }

    /// The entry of the remote named `name`, if the store records one.
    fn find(&self, name: &str) -> Result<Option<Entry>, OpError> {
        let file = self.read_remotes()?;
        Ok(file.remotes.into_iter().find(|entry| entry.name == name))
    }

    /// `remotes.toml`: empty when there is none; `E_STORE_CORRUPT` when it is not one this build
    /// reads.
    fn read_remotes(&self) -> Result<RemotesFile, OpError> {
        let fs = self.ops.repo.store_fs();
        let Some(bytes) = fs.read(fs.root().join(REMOTES_TOML)).map_err(io)? else {
            return Ok(RemotesFile {
                version: VERSION,
                remotes: Vec::new(),
            });
        };

        // 1. Text, and the version, before the shape this version gives it.
        let text = String::from_utf8(bytes).map_err(|_| corrupt("is not UTF-8"))?;
        let versioned: Versioned = toml::from_str(&text).map_err(|e| corrupt(e.message()))?;
        if versioned.version != i64::from(VERSION) {
            let found = versioned.version;
            return Err(corrupt(&format!("has version {found}, not {VERSION}")));
        }

        // 2. The remotes, each name once.
        let file: RemotesFile = toml::from_str(&text).map_err(|e| corrupt(e.message()))?;
        let mut names = BTreeSet::new();
        for entry in &file.remotes {
            if !names.insert(entry.name.as_str()) {
                return Err(corrupt(&format!("records {:?} twice", entry.name)));
            }
        }
        Ok(file)
    }

    /// Writes `file` whole, durably, through the store's filesystem seam.
    fn write_remotes(&self, file: &RemotesFile) -> Result<(), OpError> {
        let body = toml::to_string(file).map_err(|e| {
            error::error(
                ErrorCode::Internal,
                format!("{REMOTES_TOML} cannot be written: {e}"),
            )
        })?;
        let text = format!("{HEADER}{body}");
        let fs = self.ops.repo.store_fs();
        fs.atomic_replace(fs.root().join(REMOTES_TOML), text.as_bytes())
            .map_err(io)
    }
}

/// `mirror` made or rebuilt under `lock`, its stale git lock files removed; with the warnings
/// each step gives.
fn ready(mirror: Mirror, lock: &MirrorLock) -> Result<(Mirror, Vec<String>), OpError> {
    let mut warnings = Vec::new();

    // 1. Made, kept, or set aside and made again.
    let rebuilt = mirror.ensure(lock).map_err(error::mirror_error)?;
    if let Rebuilt::SetAside { name } = rebuilt {
        warnings.push(format!("mirror_set_aside: git/{name}"));
    }

    // 2. A dead process's git lock files.
    let removed = mirror.sweep_git_locks(lock).map_err(error::mirror_error)?;
    let removed = removed
        .into_iter()
        .map(|file| format!("stale_git_lock_removed: {file}"));
    warnings.extend(removed);
    Ok((mirror, warnings))
}

/// The remote's default branch (its `HEAD`), or [`FALLBACK_BRANCH`] with the warning
/// [`BRANCH_UNKNOWN`] when it names none.
fn default_branch(
    url: &RemoteUrl,
    cred: &Credential,
    name: &str,
) -> Result<(String, Option<String>), OpError> {
    let failed = |e| {
        let refused = error::transport_error(e, name, Phase::LsRemote);
        if refused.code != ErrorCode::RemoteUnreachable.as_str() {
            return refused;
        }
        refused.with_hint("--branch <branch> records the remote without asking it")
    };
    let git = GitCli::find().map_err(failed)?;
    let head = git.ls_remote_head(url, cred).map_err(failed)?;
    let found = match head {
        Some(branch) => (branch, None),
        None => (FALLBACK_BRANCH.to_owned(), Some(BRANCH_UNKNOWN.to_owned())),
    };
    Ok(found)
}

/// What the clone's directory `at` holds.
fn place(at: &Utf8Path) -> Result<Place, OpError> {
    let entries = match std::fs::read_dir(at) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Place::Free),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => return Ok(Place::Taken),
        Err(e) => {
            return Err(error::error(
                ErrorCode::Io,
                format!("{at} cannot be read: {e}"),
            ));
        }
    };
    let taken = entries.count() > 0;
    Ok(if taken { Place::Taken } else { Place::Free })
}

/// A store made at `at` named `name`; a name the store refuses gets the hint `--name`.
fn made_store(at: &Utf8Path, name: &str) -> Result<LocalOps, OpError> {
    let with_name = "give the repository's name with --name <name>";
    if name.is_empty() {
        return Err(
            error::error(ErrorCode::InvalidArg, "the URL names no repository")
                .with_details(error::details([("reason", "name".into())]))
                .with_hint(with_name),
        );
    }
    let made = LocalOps::init(at, name).map_err(|e| {
        if e.code == ErrorCode::InvalidArg.as_str() {
            e.with_hint(with_name)
        } else {
            e
        }
    })?;
    Ok(made.0)
}

/// The store at `at`, which a clone of `add` resumes into: one that records `add`'s remote at
/// the same URL, or that records no remote and no ref. Anything else is `E_EXISTS`.
fn resumable(at: &Utf8Path, add: &RemoteAddReq) -> Result<LocalOps, OpError> {
    let not_empty = || not_empty(at);
    let ops = LocalOps::open(at).map_err(|_| not_empty())?;

    // Its remotes and refs, read as any command reads them.
    let resolved = Resolved::default();
    let recording = ops.recording(&resolved);
    let remotes = recording.read_remotes()?.remotes;
    let own = remotes
        .iter()
        .any(|entry| entry.name == add.name && entry.url == add.url);
    let blank = remotes.is_empty() && ops.repo.list_refs().map_err(error::op)?.is_empty();
    if own || blank {
        return Ok(ops);
    }
    Err(not_empty())
}

/// `E_EXISTS`: the clone's directory `at` holds something a clone does not resume into.
fn not_empty(at: &Utf8Path) -> OpError {
    error::error(
        ErrorCode::ExistsAlready,
        format!("{at} is not empty, and holds no clone of this remote to resume"),
    )
    .with_hint(error::hint(format!(
        "swarmops clone <url> --into <an empty or new directory>, or swarmops --repo {} info",
        error::shell_word(at.as_str())
    )))
}

/// `e`, from a clone that made its store at `at` first: it says the store is there
/// (`details.created: true`), and that the same clone run again resumes.
fn created(at: &Utf8Path, mut e: OpError) -> OpError {
    e.details.insert("created".to_owned(), true.into());
    let again = format!(
        "the store at {} is made: run the same clone again to resume it",
        error::shell_word(at.as_str())
    );
    e.hint = Some(e.hint.take().unwrap_or(again));
    e
}

/// The repository's name in `url`: its last path component, without `.git`.
fn repo_name(url: &str) -> String {
    let path = url.trim_end_matches('/');
    let last = path.rsplit(['/', ':']).next().unwrap_or_default();
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

/// Refuses a remote name the rules do not take (`E_INVALID_ARG` `reason: "name"`):
/// `[a-z0-9][a-z0-9._-]{0,62}`, not ending in `.`, not `status`, and portable.
fn check_name(name: &str) -> Result<(), OpError> {
    let first = name.bytes().next();
    let starts = first.is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let rest = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
    let grammar = starts && rest && name.len() <= MAX_NAME_LEN && !name.ends_with('.');
    let portable = swarmops_core::portable::check_component(name).is_ok();
    if grammar && portable && name != SUBCOMMAND {
        return Ok(());
    }
    let refused = error::error(
        ErrorCode::InvalidArg,
        format!(
            "{name:?} is no remote name: [a-z0-9][a-z0-9._-]{{0,62}}, not ending in ., not status"
        ),
    );
    Err(refused
        .with_details(error::details([
            ("reason", "name".into()),
            ("name", name.into()),
        ]))
        .with_hint("name the remote origin, upstream or another lower-case word"))
}

/// Refuses a branch name git would not take, or that could be read as an option
/// (`E_INVALID_ARG` `reason: "branch"`).
fn check_branch(branch: &str) -> Result<(), OpError> {
    let bad_char = |c: char| c.is_control() || " ~^:?*[\\".contains(c);
    let bad = branch.is_empty()
        || branch.len() > MAX_LEN
        || branch.starts_with(['-', '.', '/'])
        || branch.ends_with(['/', '.'])
        || branch.ends_with(LOCK_SUFFIX)
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains("@{")
        || branch.contains("/.")
        || branch.chars().any(bad_char);
    if !bad {
        return Ok(());
    }
    let refused = error::error(
        ErrorCode::InvalidArg,
        format!("{branch:?} is no branch name git takes"),
    );
    Err(refused
        .with_details(error::details([("reason", "branch".into())]))
        .with_hint("give the remote branch by its short name, such as main"))
}

/// Refuses a token variable's name that is no environment variable's
/// (`E_INVALID_ARG` `reason: "token_env"`).
fn check_token_env(var: Option<&str>) -> Result<(), OpError> {
    let Some(var) = var else {
        return Ok(());
    };
    let first = var.bytes().next();
    let starts = first.is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
    let rest = var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if starts && rest && var.len() <= MAX_LEN {
        return Ok(());
    }
    let refused = error::error(
        ErrorCode::InvalidArg,
        "--token-env names an environment variable, such as GH_TOKEN, never the token",
    );
    Err(refused.with_details(error::details([("reason", "token_env".into())])))
}

/// `E_EXISTS`: the store records a remote named `name` already.
fn remote_exists(name: &str) -> OpError {
    error::error(
        ErrorCode::ExistsAlready,
        format!("the store records a remote {name:?} already"),
    )
    .with_details(error::details([("remote", name.into())]))
    .with_hint(error::hint(format!(
        "swarmops remote remove {} first, or give another name",
        error::shell_word(name)
    )))
}

/// `E_STORE_CORRUPT` for `remotes.toml`, which `what`.
fn corrupt(what: &str) -> OpError {
    error::error(ErrorCode::StoreCorrupt, format!("{REMOTES_TOML} {what}"))
        .with_details(error::details([("store_file", REMOTES_TOML.into())]))
        .with_hint("fix or remove remotes.toml, then swarmops remote add the remotes again")
}

/// The operations-spec error for a filesystem failure on `remotes.toml`.
fn io(e: IoFailure) -> OpError {
    error::op(RepoError::from(StoreError::from(e)))
}

/// The two refspecs of a fetch of `remote`: its branches and its tags, each into its own
/// place in the mirror.
fn refspecs(remote: &str) -> [RefSpec; 2] {
    [
        RefSpec::new(
            "refs/heads/*".to_owned(),
            format!("refs/remotes/{remote}/*"),
        ),
        RefSpec::new(
            "refs/tags/*".to_owned(),
            format!("refs/remote-tags/{remote}/*"),
        ),
    ]
}

/// `refs/remotes/<remote>/<branch>`, what the last fetch saw of the branch.
fn tracking_ref(remote: &str, branch: &str) -> String {
    format!("refs/remotes/{remote}/{branch}")
}

/// A head as a fetch reports it: the commit and its changeset.
fn remote_head(oid: GitOid, changeset: ChangesetId) -> RemoteHead {
    RemoteHead::new(oid.to_string(), changeset.to_string())
}

/// Each branch that moved, appeared or went between `before` and `after`, by name, at most
/// [`MAX_FETCH_BRANCHES`]; and whether more did.
fn moved_branches(
    before: &BTreeMap<String, GitOid>,
    after: &BTreeMap<String, GitOid>,
) -> (Vec<BranchUpdate>, bool) {
    let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
    names.sort();
    names.dedup();

    // One update a branch whose value differs.
    let updates = names.into_iter().filter_map(|branch| {
        let old = before.get(branch);
        let new = after.get(branch);
        (old != new).then(|| {
            let mut update = BranchUpdate::new(branch.clone());
            update.old = old.map(GitOid::to_string);
            update.new = new.map(GitOid::to_string);
            update
        })
    });
    let mut moved: Vec<BranchUpdate> = updates.collect();
    let truncated = moved.len() > MAX_FETCH_BRANCHES;
    moved.truncate(MAX_FETCH_BRANCHES);
    (moved, truncated)
}

/// How many entries of `after` are new or name another object than in `before`.
fn changed(before: &BTreeMap<String, GitOid>, after: &BTreeMap<String, GitOid>) -> u64 {
    let count = after
        .iter()
        .filter(|(name, oid)| before.get(*name) != Some(*oid))
        .count();
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The import's set of the wire's `classes`, by name.
fn lossy_set(classes: &[LossyClass]) -> LossySet {
    let by_name = |class: &LossyClass| {
        swarmops_git::LossyClass::ALL
            .into_iter()
            .find(|import| import.as_str() == class.as_str())
    };
    classes.iter().filter_map(by_name).collect()
}

#[cfg(test)]
mod tests {
    use super::{check_branch, check_name, repo_name};

    /// The repository's name is the URL's last path component, without `.git`, in each of the
    /// URL forms a remote may have.
    #[test]
    fn repo_name_is_the_last_component() {
        let urls = [
            ("https://github.com/acme/widgets.git", "widgets"),
            ("https://github.com/acme/widgets/", "widgets"),
            ("ssh://git@github.com/acme/widgets", "widgets"),
            ("git@github.com:acme/widgets.git", "widgets"),
            ("git@github.com:widgets.git", "widgets"),
            ("https://github.com/", "github.com"),
        ];
        for (url, name) in urls {
            assert_eq!(repo_name(url), name, "{url}");
        }
    }

    /// The name rule takes the usual names and refuses the rest, `status` among them.
    #[test]
    fn remote_names() {
        for name in ["origin", "upstream", "a", "gh-1", "my_remote.2"] {
            assert!(check_name(name).is_ok(), "{name}");
        }
        let long = "a".repeat(64);
        for name in [
            "status",
            "",
            "Origin",
            "-a",
            "a.",
            "a/b",
            "con",
            long.as_str(),
        ] {
            assert!(check_name(name).is_err(), "{name:?}");
        }
    }

    /// The branch rule refuses what git refuses and what could be an option.
    #[test]
    fn branch_names() {
        for branch in ["main", "release/1.0", "feat/x-y"] {
            assert!(check_branch(branch).is_ok(), "{branch}");
        }
        for branch in [
            "", "-f", "a..b", "a b", "x.lock", "a/", "a@{1}", ".a", "a/.b",
        ] {
            assert!(check_branch(branch).is_err(), "{branch:?}");
        }
    }
}
