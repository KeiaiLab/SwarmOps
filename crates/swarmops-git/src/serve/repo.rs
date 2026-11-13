//! The serving repository, `<store>/git/serve.git` (M14 design §0.2): bare, SHA-1, written by
//! SwarmOps and host git alone, apart from the M13 mirror, with its own lock and the
//! `pre-receive` and `proc-receive` hooks in `<store>/git/serve-hooks/`. A cache: one that is
//! missing is made again and exported again.
//!
//! ```text
//! <store>/git/
//!   SERVE                     "swarmops-git-serve 1\n": the layout's version
//!   serve.lock                the lock a ref of serve.git moves under (waited for, bounded)
//!   serve.git/                bare, SHA-1, HEAD → refs/heads/main
//!   serve.git.broken-<ms>     one set aside: an unknown marker, or one gix could not open
//!   serve-hooks/pre-receive   PRE_RECEIVE_SCRIPT, mode 0755, replaced whole when it differs
//!   serve-hooks/proc-receive  HOOK_SCRIPT, the same
//! ```
//!
//! [`ServeRepo::open`] looks without the lock first: a repository under the known marker that
//! gix opens, with the hooks as they should be, is taken as it is. Anything else is made, set aside
//! or replaced under the lock, after a second look, so that two connections opening at once do
//! the work once. The lock is waited for, up to what the caller gives ([`ServeRepo::lock`]):
//! unlike the mirror's, which a fetch holds for its whole run, it is held only while refs move.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, SendError};
use std::thread;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};

use crate::gix_sink::{self, GixSink};
use crate::mirror::{STALE_LOCK_AGE, aside_name, exists, holds_marker, make_dir, sync_dir};
use crate::oid::GitOid;
use crate::sink::{BranchUpdate, SinkError};

/// The bytes of `<store>/git/SERVE`, the serving repository's layout and its version.
pub const SERVE_MARKER: &[u8] = b"swarmops-git-serve 1\n";

/// What a push's hook file says to the client when its program cannot start.
macro_rules! push_hook_gone {
    () => {
        "swarmops serve: the hook program cannot start; the push is refused"
    };
}

/// What a fetch's `pack-objects` hook says to the client when its program cannot start.
macro_rules! fetch_hook_gone {
    () => {
        "swarmops serve: the hook program cannot start; the fetch is refused"
    };
}

/// The test a hook makes before it hands over: the serving binary is a file it may run.
macro_rules! program_there {
    () => {
        "[ -f \"$SWARMOPS_SERVE_EXE\" ] && [ -x \"$SWARMOPS_SERVE_EXE\" ]"
    };
}

/// The line of a push's hook file whose program cannot start ([`HOOK_SCRIPT`]).
pub const PUSH_HOOK_GONE: &str = push_hook_gone!();

/// The line of a fetch's hook whose program cannot start ([`PACK_OBJECTS_HOOK`]).
pub const FETCH_HOOK_GONE: &str = fetch_hook_gone!();

/// The hook file `<store>/git/serve-hooks/proc-receive`. git runs a hook as a file, so this
/// one is POSIX `sh`: it hands the hook to the binary that serves, which the parent named.
///
/// git sends what a hook writes on stderr to the client. When the binary is gone (replaced
/// while it served), is no file or is not executable, `sh`'s own error would name this file
/// under the store and the binary, so the script looks first, says [`PUSH_HOOK_GONE`], which
/// names no path, and exits 1: git refuses the push.
pub const HOOK_SCRIPT: &str = concat!(
    "#!/bin/sh\n",
    "# git runs hooks as files: this one hands proc-receive to the swarmops that serves.\n",
    "if ",
    program_there!(),
    "; then\n",
    "  exec \"$SWARMOPS_SERVE_EXE\" serve --hook proc-receive\n",
    "fi\n",
    "# No program to run: serve's own line, since sh's would name host paths to the client.\n",
    "echo '",
    push_hook_gone!(),
    "' >&2\n",
    "exit 1\n",
);

/// The hook file `<store>/git/serve-hooks/pre-receive`, as [`HOOK_SCRIPT`]: git runs it while
/// the push's objects are still in its quarantine, so that a refusal keeps them out.
pub const PRE_RECEIVE_SCRIPT: &str = concat!(
    "#!/bin/sh\n",
    "# git runs hooks as files: this one hands pre-receive to the swarmops that serves.\n",
    "if ",
    program_there!(),
    "; then\n",
    "  exec \"$SWARMOPS_SERVE_EXE\" serve --hook pre-receive\n",
    "fi\n",
    "# No program to run: serve's own line, since sh's would name host paths to the client.\n",
    "echo '",
    push_hook_gone!(),
    "' >&2\n",
    "exit 1\n",
);

/// The command git runs instead of `pack-objects` when it answers a fetch (its
/// `uploadpack.packObjectsHook`, set in the child's environment, which git counts as protected
/// configuration): git runs it through `sh` and appends the `git pack-objects …` line it would
/// run as the shell's arguments, and the binary that serves answers from its pack cache (the
/// M17 design §5). It is a shell function, one line, so that it can look for the binary first
/// as [`HOOK_SCRIPT`] does: git sends this command's stderr to the client too, and it says
/// [`FETCH_HOOK_GONE`] and exits 1 instead of `sh`'s error, which would name the binary.
pub const PACK_OBJECTS_HOOK: &str = concat!(
    "swarmops_pack_objects() { if ",
    program_there!(),
    "; then exec \"$SWARMOPS_SERVE_EXE\" serve --hook pack-objects \"$@\"; fi; echo '",
    fetch_hook_gone!(),
    "' >&2; exit 1; }; swarmops_pack_objects",
);

/// The store's git directory, which the mirror shares: `<store>/git`.
const GIT: &str = "git";

/// The marker file, the lock file and the repository, in `git/`.
const MARKER_FILE: &str = "SERVE";
const LOCK_FILE: &str = "serve.lock";
const SERVE_GIT: &str = "serve.git";

/// The prefix of the name a broken repository is set aside as, in `git/`.
const BROKEN: &str = "serve.git.broken-";

/// The hooks directory, in `git/`, and its hooks: each file's name and bytes.
const HOOKS: &str = "serve-hooks";
const HOOK_FILES: [(&str, &str); 2] = [
    ("pre-receive", PRE_RECEIVE_SCRIPT),
    ("proc-receive", HOOK_SCRIPT),
];

/// The hook's mode: git runs it, so it is executable.
const HOOK_MODE: u32 = 0o755;

/// What `HEAD` holds: the branch a clone checks out.
const HEAD: &[u8] = b"ref: refs/heads/main\n";

/// How long [`ServeRepo::open`] waits for the lock to make or repair the repository: the
/// store's writer lock wait (the limits page's "Writer lock wait").
const REPAIR_WAIT: Duration = Duration::from_secs(10);

/// A store's serving repository.
#[derive(Debug)]
#[non_exhaustive]
pub struct ServeRepo {
    /// `<store>/git/serve.git`.
    git_dir: Utf8PathBuf,
    /// `<store>/git/serve-hooks`.
    hooks_dir: Utf8PathBuf,
}

/// The serving repository's lock, held until dropped: the proof a ref moves under it.
#[derive(Debug)]
#[non_exhaustive]
pub struct ServeLock {
    /// The open lock file, which holds the lock.
    _file: File,
}

/// Why the serving repository failed. Its texts name files relative to the store's `git/`,
/// never a host path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// The part of `serve` this is has not been built yet.
    #[error("not built")]
    NotBuilt,
    /// Another connection held the lock for longer than the wait (`E_BUSY`).
    #[error("another connection holds git/serve.lock")]
    Busy,
    /// The filesystem refused an operation on a file of `git/`.
    #[error("git/{what}: {reason}")]
    Io {
        /// The file or directory, relative to `git/`.
        what: String,
        /// What failed.
        reason: String,
    },
    /// Reading or writing `serve.git` failed.
    #[error("serve.git: {0}")]
    Sink(#[from] SinkError),
}

impl ServeRepo {
    /// The serving repository of the store at `store_root`, made when it is missing: `git/`,
    /// `git/serve.lock`, then, under the lock when anything is to do, `serve.git` (set aside as
    /// `serve.git.broken-<unix ms>` first when its marker is unknown or gix cannot open it), the
    /// marker, and the hook (replaced whole when its bytes or mode differ). Everything made is
    /// durable.
    pub fn open(store_root: &Utf8Path) -> Result<ServeRepo, ServeError> {
        let git = store_root.join(GIT);
        let repo = ServeRepo {
            git_dir: git.join(SERVE_GIT),
            hooks_dir: git.join(HOOKS),
        };

        // 1. `git/` and the lock file, each durable in its directory when made.
        if make_dir(git.as_std_path()).map_err(io_failed(""))? {
            sync_dir(store_root.as_std_path()).map_err(io_failed(""))?;
        }
        let lock = git.join(LOCK_FILE);
        if !exists(lock.as_std_path()).map_err(io_failed(LOCK_FILE))? {
            open_lock(lock.as_std_path()).map_err(io_failed(LOCK_FILE))?;
            sync_dir(git.as_std_path()).map_err(io_failed(""))?;
        }

        // 2. A look without the lock, then the work under it, after a second look.
        if repo.sound()? {
            return Ok(repo);
        }
        let held = repo.lock(REPAIR_WAIT)?;
        repo.repair(&held)?;
        Ok(repo)
    }

    /// `<store>/git/serve.git`.
    pub fn git_dir(&self) -> &Utf8Path {
        &self.git_dir
    }

    /// `<store>/git/serve-hooks`, its `core.hooksPath`.
    pub fn hooks_dir(&self) -> &Utf8Path {
        &self.hooks_dir
    }

    /// Every ref it holds, by its full name; symbolic ones (`HEAD`) are passed over.
    pub fn refs(&self) -> Result<BTreeMap<String, GitOid>, ServeError> {
        let sink = GixSink::open(self.git_dir.as_std_path())?;
        let listed = sink.references_under("refs/")?;
        Ok(listed.into_iter().collect())
    }

    /// Takes its lock, waiting at most `wait`, then [`ServeError::Busy`]; a zero `wait` gives up
    /// at once. The wait is the kernel's (`flock`), on a helper thread, as the store's writer
    /// lock waits: a wait given up leaves the helper to take the lock and let it go at once.
    pub fn lock(&self, wait: Duration) -> Result<ServeLock, ServeError> {
        let path = self.git().join(LOCK_FILE);
        let failed = io_failed(LOCK_FILE);
        let file = open_lock(path.as_std_path()).map_err(&failed)?;
        match file.try_lock() {
            Ok(()) => return Ok(ServeLock { _file: file }),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(failed(error)),
        }
        if wait.is_zero() {
            return Err(ServeError::Busy);
        }

        // The blocking wait on a helper thread, handed back over a channel.
        let (handoff, waiting) = mpsc::sync_channel(1);
        let helper = thread::Builder::new().name("serve-lock-wait".to_owned());
        let spawned = helper.spawn(move || {
            let locked = file.lock().map(|()| ServeLock { _file: file });
            if let Err(SendError(abandoned)) = handoff.send(locked) {
                // The caller gave up: dropping the lock lets it go.
                drop(abandoned);
            }
        });
        spawned.map_err(&failed)?;
        match waiting.recv_timeout(wait) {
            Ok(locked) => locked.map_err(&failed),
            Err(RecvTimeoutError::Timeout) => Err(ServeError::Busy),
            Err(RecvTimeoutError::Disconnected) => Err(failed(io::Error::other(
                "the thread waiting for it ended without it",
            ))),
        }
    }

    /// Moves the ref `name` from `expect` to `new` by compare-and-swap (`None`: absent), under
    /// `lock`, durably: [`BranchUpdate::Mismatch`] with what it names when that is not `expect`,
    /// and nothing moves. `new` must be in the repository.
    pub fn set_ref(
        &self,
        lock: &ServeLock,
        name: &str,
        expect: Option<GitOid>,
        new: Option<GitOid>,
    ) -> Result<BranchUpdate, ServeError> {
        let _ = lock;
        let mut sink = GixSink::open(self.git_dir.as_std_path())?;
        Ok(sink.cas_ref(name, expect, new)?)
    }

    /// Removes the git lock files older than the mirror's [`STALE_LOCK_AGE`] that a dead
    /// process left in `serve.git`, under `lock`, and gives their names; a younger one is
    /// [`SinkError::RefLocked`], and then none is removed.
    pub fn sweep_git_locks(&self, lock: &ServeLock) -> Result<Vec<String>, ServeError> {
        let _ = lock;
        let removed = gix_sink::remove_stale_locks(self.git_dir.as_std_path(), STALE_LOCK_AGE)?;
        Ok(removed)
    }

    /// `git/`, which holds the marker, the lock, `serve.git` and the hooks.
    fn git(&self) -> &Utf8Path {
        self.git_dir.parent().expect("serve.git is in git/")
    }

    /// Whether the repository is as [`ServeRepo::open`] leaves it: the known marker, a
    /// `serve.git` gix opens, and each hook's bytes and mode.
    fn sound(&self) -> Result<bool, ServeError> {
        let marked = read_marker(self.git().as_std_path()).map_err(io_failed(MARKER_FILE))?;
        let opens = GixSink::open(self.git_dir.as_std_path()).is_ok();
        if !(marked && opens) {
            return Ok(false);
        }
        for (name, script) in HOOK_FILES {
            if !self.hook_sound(name, script)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the hook file `name` holds `script` with [`HOOK_MODE`].
    fn hook_sound(&self, name: &str, script: &str) -> Result<bool, ServeError> {
        let path = self.hooks_dir.join(name);
        let found = match fs::symlink_metadata(&path) {
            Ok(found) => found,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_failed(&hook_name(name))(error)),
        };
        let mode_ok = found.is_file() && found.permissions().mode() & 0o777 == HOOK_MODE;
        let bytes = fs::read(&path).map_err(io_failed(&hook_name(name)))?;
        Ok(mode_ok && bytes == script.as_bytes())
    }

    /// Makes, sets aside or replaces what is not sound, under the lock: `serve.git` and its
    /// marker, then the hook.
    fn repair(&self, _lock: &ServeLock) -> Result<(), ServeError> {
        let git = self.git().as_std_path();
        let marked = read_marker(git).map_err(io_failed(MARKER_FILE))?;
        let there = exists(self.git_dir.as_std_path()).map_err(io_failed(SERVE_GIT))?;
        let opens = GixSink::open(self.git_dir.as_std_path()).is_ok();

        // 1. serve.git: kept, or set aside and made again, then marked; a crash before the
        // marker leaves one the next connection sets aside again.
        if !(marked && there && opens) {
            if there {
                self.set_aside()?;
            }
            GixSink::create_bare(self.git_dir.as_std_path())?;
            write_head(self.git_dir.as_std_path()).map_err(io_failed("serve.git/HEAD"))?;
            write_atomic(git, MARKER_FILE, SERVE_MARKER, None).map_err(io_failed(MARKER_FILE))?;
        }

        // 2. The hooks: their directory, then each file replaced whole when it differs.
        if make_dir(self.hooks_dir.as_std_path()).map_err(io_failed(HOOKS))? {
            sync_dir(git).map_err(io_failed(""))?;
        }
        for (name, script) in HOOK_FILES {
            if self.hook_sound(name, script)? {
                continue;
            }
            let mode = Some(Permissions::from_mode(HOOK_MODE));
            write_atomic(self.hooks_dir.as_std_path(), name, script.as_bytes(), mode)
                .map_err(io_failed(&hook_name(name)))?;
        }
        Ok(())
    }

    /// Renames `serve.git` aside as `serve.git.broken-<unix ms>`, a name not yet taken; never
    /// deletes it.
    fn set_aside(&self) -> Result<(), ServeError> {
        let git = self.git().as_std_path();
        let name = aside_name(git, BROKEN).map_err(io_failed(SERVE_GIT))?;
        fs::rename(self.git_dir.as_std_path(), git.join(&name)).map_err(io_failed(SERVE_GIT))?;
        sync_dir(git).map_err(io_failed(""))
    }
}

/// `serve-hooks/<name>`, as an error names it.
fn hook_name(name: &str) -> String {
    format!("{HOOKS}/{name}")
}

/// The [`ServeError::Io`] of `what`, relative to `git/`, for an io error.
fn io_failed(what: &str) -> impl Fn(io::Error) -> ServeError {
    let what = what.to_owned();
    move |error| ServeError::Io {
        what: what.clone(),
        reason: error.kind().to_string(),
    }
}

/// Opens the lock file at `path` for writing, making it when it is missing.
fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Whether `git/SERVE` holds this layout's marker; a missing one does not.
fn read_marker(git: &Path) -> io::Result<bool> {
    holds_marker(&git.join(MARKER_FILE), SERVE_MARKER)
}

/// Writes `HEAD` of the bare repository `git_dir` as [`HEAD`] says, whatever gix wrote.
fn write_head(git_dir: &Path) -> io::Result<()> {
    write_atomic(git_dir, "HEAD", HEAD, None)
}

/// Replaces `dir/name` with `bytes` whole: a temporary file in `dir`, given `mode`, synced, then
/// renamed onto it, and `dir` synced.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8], mode: Option<Permissions>) -> io::Result<()> {
    let temp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    if let Some(mode) = mode {
        file.set_permissions(mode)?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, dir.join(name))?;
    sync_dir(dir)
}
