//! `update`: the program replaced by the latest release, and the switch of automatic update.
//!
//! It opens no store: what it reads and writes is beside the binaries, in the directory the
//! running program is installed in, so `--repo` and `--as` are read and not used, and the
//! envelope has no `meta.repo`. What each answer means is `swarmops_ops::update`'s to say; this
//! module reads the files, runs curl and the installer, and replaces the binaries.
//!
//! ```text
//!   update ── eligible? ── lock ── curl: HEAD releases/latest ──► github.com
//!                │           │      ◄── 302 Location: …/releases/tag/v<L>
//!                │           ├── <dir>/swarmops --version, <dir>/so --version: the lower one
//!                │           │   is installed; L not newer: done, nothing replaced
//!                │           ├── /bin/sh -s -- --version <L> --dir <dir>/.swarmops-update.d
//!                │           │   (the installer built in, on stdin: download, SHA256SUMS, unpack)
//!                │           ├── the staged swarmops and so each print `swarmops <L>`
//!                │           └── rename swarmops, rename so, fsync <dir>, remove the staging
//!                └── no release's build, or no receipt: E_INVALID_ARG
//! ```
//!
//! The forms that reach no network: `--status` says whether the install updates itself
//! (`swarmops_ops::update::auto_update`) and gives the note no command has printed yet;
//! `--auto on|off` writes the receipt's `auto_update`, by a temporary file renamed over it.
//!
//! Automatic update is what follows a command that went well, once its envelope is out
//! ([`after_command`]): where the install updates itself, the command prints the note an earlier
//! run left, if its stderr is a terminal, and once a day starts `update --background`, detached,
//! which nobody waits for.
//!
//! ```text
//!   a command, exit 0 ── envelope out ── after_command
//!        a release's build, a receipt that says true, no switch, CI or container, the owner
//!        ├── stderr a terminal, a note: unlink it, and print it
//!        ├── the lock file younger than 24 h: done
//!        ├── claim: lock, look again, set its time to now, unlock
//!        └── /bin/sh -c 'exec <program> update --background 3>&- … 9>&-'
//!            (its own process group, no stdio, the caller's descriptors 3 to 9 closed)
//! ```
//!
//! `--background` is that run. It does nothing unless automatic update is on, takes the lock,
//! waiting no longer than a command's claim holds it, and installs the latest release only when
//! the release holds
//! the mark for the installed minor line: one more HEAD, which answers 302 for a file the
//! release holds and 404 for one it does not. Any other answer, like every failure to look, is
//! left for the next check and says nothing. What it did is one line in the note file, for the
//! next command at a terminal, and what it must remember is in the lock file
//! (`swarmops_ops::update::State`): the release seen unmarked, announced, or failed.
//!
//! The installer runs with a cleared environment but for `PATH`, `HOME`, `TMPDIR`, the proxies
//! and the certificate stores, so that no `SWARMOPS_*` variable of the caller's names another
//! version, directory or address to it; in a process group of its own, which is stopped at a
//! deadline; and with its output read and dropped, but for the last line it said, which a
//! failure's error carries. The staged binaries are run with every `SWARMOPS_*` variable removed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek as _, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::process::{Pid, Signal};
use serde_json::{Map, Value};
use swarmops_api::{AutoUpdate, ErrorCode, OpError, UpdateOut};
use swarmops_ops::update::{
    self, CONTAINER_MARKERS, Facts, Ineligible, LOCK_FILE, NOTE_FILE, Note, RECEIPT_FILE,
    RELEASES_URL, Receipt, STAGING_DIR, State, Switch, Version,
};

use super::Ctx;
use super::device_flow::{self, CURL_ENV_KEPT, Caller, Curl, Failed, Redirect};
use crate::cli::AutoArg;
use crate::context::Env;
use crate::output::{self, Printed};
use crate::version;

/// The installer, as this build's source holds it: what `update` runs to download a release,
/// check it against its SHA256SUMS and unpack it.
const INSTALLER: &str = include_str!("../../../../scripts/install.sh");

/// The shell the installer runs under.
const SHELL: &str = "/bin/sh";

/// How each line the installer says on stderr starts.
const INSTALLER_PREFIX: &str = "swarmops-install: ";

/// The variables the installer inherits besides curl's ([`CURL_ENV_KEPT`]): where a default
/// directory and its temporary directory are.
const INSTALLER_ENV_KEPT: [&str; 2] = ["HOME", "TMPDIR"];

/// The two names of the program, in the order they are replaced: the installer's.
const NAMES: [&str; 2] = ["swarmops", "so"];

/// How long the installer may run for an `update` the user ran, and for one in the background,
/// which may hold what its parent's caller passed it.
const EXPLICIT_DEADLINE: Duration = Duration::from_mins(10);
const BACKGROUND_DEADLINE: Duration = Duration::from_mins(3);

/// How long a program asked for its version may run.
const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// How long a child stopped at its deadline has to end before it is killed.
const KILL_AFTER: Duration = Duration::from_secs(5);

/// How long a child's output is waited for once it has ended: a process it left behind may
/// hold the pipe.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// The longest wait between two looks at a child.
const MAX_NAP: Duration = Duration::from_millis(50);

/// The most bytes kept of what a child printed, and read of a file beside the binaries.
const MAX_KEPT_BYTES: usize = 64 * 1024;

/// The most characters of a program's answer an error quotes.
const MAX_QUOTED_CHARS: usize = 200;

/// The mode of the files written beside the binaries: the installer's for its receipt.
const FILE_MODE: u32 = 0o644;

/// How long a run in the background waits for the lock: a command that claims the day's check
/// ([`Install::claim`]) holds it for an instant, and may do so as the run it started begins.
const CLAIM_WAIT: Duration = Duration::from_millis(500);

/// The shell's line that becomes the run in the background: the program, `$0`, in place of the
/// shell, without the descriptors 3 to 9 its parent's caller may have passed down. A lock a
/// script holds on one (`flock 9`), or a pipe a harness reads to its end, is then not held for
/// as long as a download takes. A descriptor that is not open is closed without complaint.
const DETACHED: &str = r#"exec "$0" update --background 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-"#;

/// What runs curl here, for the refusal where there is none.
const UPDATE: Caller = Caller {
    what: "update",
    without: None,
};

/// What `update` is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Form {
    /// Install the latest release if it is newer.
    Install,
    /// `--check`: look for the latest release, and install nothing.
    Check,
    /// `--auto on|off`: write the receipt's `auto_update`.
    Auto(bool),
    /// `--status`: say whether the install updates itself.
    Status,
    /// `--background`: what automatic update runs.
    Background,
}

impl Form {
    /// The form the flags name; the grammar lets at most one be given.
    pub(super) fn of(check: bool, auto: Option<AutoArg>, status: bool, background: bool) -> Form {
        match auto {
            Some(auto) => Form::Auto(auto == AutoArg::On),
            None if check => Form::Check,
            None if status => Form::Status,
            None if background => Form::Background,
            None => Form::Install,
        }
    }
}

/// Runs `update` in the form `form`, which names operation `op`.
pub(super) fn run(ctx: &Ctx<'_>, op: &'static str, form: Form) -> Printed {
    ctx.finish(op, (None, BTreeMap::new(), None), update(ctx.env, form))
}

/// `update` in the form `form`.
fn update(env: Env<'_>, form: Form) -> Result<UpdateOut, OpError> {
    let found = Found::look(env)?;
    match form {
        Form::Status => Ok(found.out()),
        Form::Auto(on) => set_auto(env, found, on),
        Form::Check => check(env, &found),
        Form::Install => install(env, &found),
        Form::Background => Ok(background(env, &found)),
    }
}

/// What follows a command that went well, once its envelope is out: automatic update. Nothing
/// it finds or does is an error, and nothing is waited for. `terminal` says whether stderr,
/// which `log` writes to, is one.
///
/// 1. A build that is no release's stops here, before any file is looked at.
/// 2. So does an install that does not update itself (`swarmops_ops::update::auto_update`, as
///    `--status` says it), or whose receipt is refused.
/// 3. At a terminal, the note an earlier run left is removed and printed, by the one command
///    whose removal went through.
/// 4. A check claimed less than a day ago ends it. Otherwise this command claims today's
///    ([`Install::claim`]) and starts `update --background`, detached ([`spawn_background`]).
pub(crate) fn after_command(env: Env<'_>, terminal: bool, log: &mut dyn Write) {
    if !version::is_release() {
        return;
    }
    let Ok(found) = Found::look(env) else {
        return;
    };
    let (AutoUpdate::On, Some(install)) = (found.auto, &found.install) else {
        return;
    };
    if terminal {
        install.say_note(log);
    }
    if install.claim() {
        let _ = spawn_background();
    }
}

/// Starts `update --background` by this program, detached, and gives the child, which nobody
/// waits for: a process group of its own, no stdio a reader could wait on, the root for its
/// directory, so that it holds no directory of the caller's, and none of the caller's
/// descriptors 3 to 9 ([`DETACHED`]). It has the caller's environment: what turned automatic
/// update on here turns it on there. `None` when the program cannot name its own file, or
/// there is no shell.
fn spawn_background() -> Option<Child> {
    let program = std::env::current_exe().ok()?;
    Command::new(SHELL)
        .arg("-c")
        .arg(DETACHED)
        .arg(program)
        .current_dir("/")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// `update --auto on|off`: the receipt's `auto_update` written, where it says otherwise. Turning
/// it on needs what `update` needs; turning it off where there is no receipt changes nothing,
/// since nothing updates itself there.
fn set_auto(env: Env<'_>, mut found: Found, on: bool) -> Result<UpdateOut, OpError> {
    if on {
        found.eligible()?;
    }
    if let (Some(install), Some(receipt)) = (&found.install, &mut found.receipt)
        && receipt.auto_update() != on
    {
        receipt.set_auto_update(on);
        install.write_receipt(receipt)?;
        found.auto = decide(env, found.release, Some(install), Some(receipt));
    }
    Ok(found.out())
}

/// `update --check`: the latest release, and the version installed.
fn check(env: Env<'_>, found: &Found) -> Result<UpdateOut, OpError> {
    let install = found.eligible()?;
    let curl = Curl::find(env, &UPDATE)?;
    let latest = latest(&curl)?;
    let mut out = found.out();
    compared(&mut out, install.installed().as_ref(), &latest);
    Ok(out)
}

/// `update`: the latest release installed, if it is newer than what is installed.
fn install(env: Env<'_>, found: &Found) -> Result<UpdateOut, OpError> {
    // 1. What can be refused before the network: the build, the receipt, curl, another update.
    let install = found.eligible()?;
    let curl = Curl::find(env, &UPDATE)?;
    let mut lock = install.lock().map_err(|e| e.refusal(install))?;

    // 2. The latest release against the lower of the two names' versions.
    let latest = latest(&curl)?;
    let installed = install.installed();
    let mut out = found.out();
    compared(&mut out, installed.as_ref(), &latest);

    // 3. Only a strictly newer release replaces anything.
    if latest.replaces(installed.as_ref()) {
        install.replace(&latest, &installer_env(env), EXPLICIT_DEADLINE)?;
        out.previous = installed.map(|version| version.to_string());
        out.current = latest.to_string();
        out.updated = true;
        // The note was of an older state of the install: the envelope says the new one.
        install.forget_note();
        out.pending = None;
    }

    // 4. The user has updated: a release that failed automatically may be tried again.
    let mut state = lock.state();
    if state.forget_failure() {
        let _ = lock.write(&state);
    }
    Ok(out)
}

/// `update --background`: with automatic update on, the latest release installed if it is newer
/// and marked for the installed line. Whatever stops it short of that changes nothing, and says
/// nothing but in what it returns.
fn background(env: Env<'_>, found: &Found) -> UpdateOut {
    let mut out = found.out();

    // 1. The gates, again: the parent decided before it started this run.
    let (AutoUpdate::On, Some(install)) = (found.auto, &found.install) else {
        return out;
    };
    let Ok(curl) = Curl::find(env, &UPDATE) else {
        return out;
    };
    let Ok(mut lock) = install.lock_within(CLAIM_WAIT) else {
        return out;
    };

    // 2. The latest release: not newer, or failed before, and there is nothing to do.
    let Ok(latest) = latest(&curl) else {
        return out;
    };
    let installed = install.installed();
    compared(&mut out, installed.as_ref(), &latest);
    let mut state = lock.state();
    let before = state.clone();
    if !latest.replaces(installed.as_ref()) || state.has_failed(&latest) {
        return out;
    }
    let Some(running) = installed.or_else(|| Version::parse(version::version())) else {
        return out;
    };

    // 3. The mark for the installed line: there, the release is installed; not there, it is
    //    announced at its second sighting; no answer, nothing.
    let note = match marked(&curl, &latest, &running) {
        Some(true) => match install.replace(&latest, &installer_env(env), BACKGROUND_DEADLINE) {
            Ok(()) => {
                out.previous = Some(running.to_string());
                out.current = latest.to_string();
                out.updated = true;
                state.forget_failure();
                Some(Note::Updated {
                    from: running,
                    to: latest,
                })
            }
            Err(error) => {
                state.record_failure(&latest);
                Some(Note::failed(latest, &why(&error)))
            }
        },
        Some(false) => state.sight_unmarked(&latest),
        None => return out,
    };

    // 4. What to remember, then what to say.
    if state != before {
        let _ = lock.write(&state);
    }
    if let Some(note) = note
        && install.leave(&note).is_ok()
    {
        out.pending = Some(note.pending());
    }
    out
}

/// Sets what a look at the latest release found: the version installed, when the two names
/// have one, the release, and its notes' page.
fn compared(out: &mut UpdateOut, installed: Option<&Version>, latest: &Version) {
    if let Some(installed) = installed {
        out.current = installed.to_string();
    }
    out.latest = Some(latest.to_string());
    out.notes = Some(update::notes_address(latest));
}

/// What a failure to install says in a note: the installer's last line, else the error's own.
fn why(error: &OpError) -> String {
    match error.details.get("installer").and_then(Value::as_str) {
        Some(line) => line.to_owned(),
        None => error.message.clone(),
    }
}

/// The latest release: the one whose page the releases page's `latest` redirects to. Any other
/// answer is `E_REMOTE_UNREACHABLE`: no release is taken from an address that is not exactly a
/// release's page.
fn latest(curl: &Curl) -> Result<Version, OpError> {
    let answer = curl
        .redirect(&update::latest_address())
        .map_err(|failed| match failed {
            Failed::Curl(error) => error,
            Failed::Network { exit } => unreachable(Unreached::Curl(exit)),
        })?;
    let Redirect {
        status: Some(301 | 302),
        to: Some(to),
    } = &answer
    else {
        return Err(unreachable(Unreached::Answer(answer.status)));
    };
    update::release_at(to).ok_or_else(|| unreachable(Unreached::Answer(answer.status)))
}

/// Whether `release` holds the mark that lets a build of `running`'s line install it
/// automatically: 302, the redirect to a file the release holds, says yes, and 404 no. `None`
/// for anything else: a host that did not answer says nothing of the release.
fn marked(curl: &Curl, release: &Version, running: &Version) -> Option<bool> {
    let answer = curl
        .redirect(&update::mark_address(release, running))
        .ok()?;
    match answer.status? {
        302 => Some(true),
        404 => Some(false),
        _ => None,
    }
}

/// The installer's environment: of all the caller's variables, curl's and
/// [`INSTALLER_ENV_KEPT`].
fn installer_env(env: Env<'_>) -> Vec<(&'static str, OsString)> {
    CURL_ENV_KEPT
        .iter()
        .chain(&INSTALLER_ENV_KEPT)
        .filter_map(|name| env(name).map(|value| (*name, value)))
        .collect()
}

/// Whether an install updates itself: `swarmops_ops::update::auto_update` of what this host
/// says. A directory whose owner cannot be read counts as another user's.
fn decide(
    env: Env<'_>,
    release: bool,
    install: Option<&Install>,
    receipt: Option<&Receipt>,
) -> AutoUpdate {
    let euid = rustix::process::geteuid().as_raw();
    let owner = install
        .and_then(|install| fs::metadata(&install.dir).ok())
        .map_or(euid ^ 1, |dir| dir.uid());
    let mut facts = Facts::new(false, euid, owner);
    facts.release_build = release;
    facts.receipt = receipt.map(Receipt::auto_update);
    facts.switch = Switch::read(env);
    facts.ci = update::in_ci(env);
    facts.container = CONTAINER_MARKERS
        .iter()
        .any(|marker| Path::new(marker).exists());
    update::auto_update(&facts)
}

/// What is known of the install before anything is done.
struct Found {
    /// Whether the build is a release's.
    release: bool,
    /// Where the running program is installed; `None` when its own file cannot be found.
    install: Option<Install>,
    /// The receipt beside the binaries.
    receipt: Option<Receipt>,
    /// Whether the install updates itself.
    auto: AutoUpdate,
}

impl Found {
    /// Looks: the build, the directory of the running program, and its receipt. A receipt that
    /// is none is `E_INVALID_ARG` (`details.reason` `receipt`, `details.path`), and so is a
    /// release's build that cannot find its own file (`exe_unknown`).
    fn look(env: Env<'_>) -> Result<Found, OpError> {
        let release = version::is_release();
        let install = Install::locate();
        if release && install.is_none() {
            let why = "the program cannot find its own file, so it cannot tell where it is \
                       installed";
            return Err(refused("exe_unknown", why));
        }
        let receipt = match &install {
            Some(install) => install.receipt()?,
            None => None,
        };
        let auto = decide(env, release, install.as_ref(), receipt.as_ref());
        Ok(Found {
            release,
            install,
            receipt,
            auto,
        })
    }

    /// The result every form starts from: this program's version, nothing replaced, and for an
    /// install with a receipt its directory and the note no command has printed.
    fn out(&self) -> UpdateOut {
        let mut out = UpdateOut::new(version::version(), false, self.auto);
        if let (Some(install), Some(_)) = (&self.install, &self.receipt) {
            out.dir = Some(install.dir.display().to_string());
            out.pending = install.note().map(|note| note.pending());
        }
        out
    }

    /// The install, when `update` may replace it: a release's build with a receipt. Else
    /// `E_INVALID_ARG` with `details.reason` `not_a_release_build` or `no_receipt`, the second
    /// with `details.dir`.
    fn eligible(&self) -> Result<&Install, OpError> {
        let ineligible = |why: Ineligible| {
            let (message, hint) = match why {
                Ineligible::SourceBuild => (
                    "this build is no release's: it was built from source, and update replaces \
                     only what the installer installed",
                    "build the newer source again, as this build was made",
                ),
                _ => (
                    "no receipt of the installer's is beside the binaries: a package, an \
                     archive unpacked by hand or an older installer put them there",
                    "update them the way they were installed; running the installer once, \
                     with --dir naming this directory, leaves a receipt",
                ),
            };
            let mut error = refused(why.reason(), message).with_hint(hint);
            if let (Ineligible::NoReceipt, Some(install)) = (why, &self.install) {
                let dir = install.dir.display().to_string();
                error.details.insert("dir".to_owned(), dir.into());
            }
            error
        };
        update::eligible(self.release, self.receipt.is_some()).map_err(ineligible)?;
        self.install
            .as_ref()
            .ok_or_else(|| ineligible(Ineligible::NoReceipt))
    }
}

/// The directory the running program is installed in, and what an update keeps there.
struct Install {
    /// The directory, with every link on the way to the program followed.
    dir: PathBuf,
}

impl Install {
    /// Where the running program is: the directory of its own file, links followed. `None`
    /// when the system does not say, or the file is gone (as after it was replaced, on Linux).
    fn locate() -> Option<Install> {
        let program = std::env::current_exe().ok()?.canonicalize().ok()?;
        Some(Install {
            dir: program.parent()?.to_path_buf(),
        })
    }

    /// The receipt beside the binaries, or `None` when there is none.
    fn receipt(&self) -> Result<Option<Receipt>, OpError> {
        let path = self.dir.join(RECEIPT_FILE);
        let bad = |why: &str| {
            let message = format!("the receipt {why}: {}", path.display());
            let hint = "it is not written over: remove it and run the installer again, which \
                        leaves a new one";
            let mut error = refused("receipt", &message).with_hint(hint);
            let shown = path.display().to_string();
            error.details.insert("path".to_owned(), shown.into());
            error
        };
        match read_bounded(&path) {
            Ok(None) => Ok(None),
            Ok(Some(text)) => match Receipt::parse(&text) {
                Ok(receipt) => Ok(Some(receipt)),
                Err(fault) => Err(bad(fault.why())),
            },
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                Err(bad("is longer than 64 KiB, or is not UTF-8"))
            }
            Err(e) => Err(
                failed_step("receipt", &format!("cannot read the receipt: {e}"))
                    .with_details(path_details("receipt", &path)),
            ),
        }
    }

    /// Writes `receipt` over the one beside the binaries: a temporary file, renamed.
    fn write_receipt(&self, receipt: &Receipt) -> Result<(), OpError> {
        let path = self.dir.join(RECEIPT_FILE);
        let failed =
            |why: String| failed_step("receipt", &why).with_details(path_details("receipt", &path));
        let text = receipt
            .text()
            .map_err(|fault| failed(format!("the receipt {}", fault.why())))?;
        replace_file(&path, &text).map_err(|e| failed(format!("cannot write the receipt: {e}")))
    }

    /// The note an automatic update left, if the file holds one.
    fn note(&self) -> Option<Note> {
        let text = read_bounded(&self.dir.join(NOTE_FILE)).ok()??;
        Note::parse(&text)
    }

    /// Leaves `note` for the next command at a terminal, over any note there.
    fn leave(&self, note: &Note) -> io::Result<()> {
        replace_file(&self.dir.join(NOTE_FILE), &note.line())
    }

    /// Removes the note, if there is one.
    fn forget_note(&self) {
        let _ = fs::remove_file(self.dir.join(NOTE_FILE));
    }

    /// Prints the note an automatic update left on `log`, a terminal, and removes it: of the
    /// commands that read it at once, the one whose removal went through prints it. A file
    /// that holds no note is left, and says nothing.
    fn say_note(&self, log: &mut dyn Write) {
        let Some(note) = self.note() else {
            return;
        };
        if fs::remove_file(self.dir.join(NOTE_FILE)).is_ok() {
            let _ = writeln!(log, "{}", note.sentence());
        }
    }

    /// Claims the check for a newer release that is due now, for the command that then starts
    /// it: whether this command has it. A check is due a day after the last was claimed
    /// (`swarmops_ops::update::check_due`), by the lock file's modification time.
    ///
    /// 1. A lock file younger than that: no. This is all most commands do.
    /// 2. No lock file: the command that makes it has the claim, since making it sets its time,
    ///    and of several only one makes it.
    /// 3. Else the file is locked without waiting (held: an update is running, or another
    ///    command claims), looked at again, and its time set to now. Now is read with the lock
    ///    held, so that a claim made meanwhile is in its past, and not a time in the future.
    ///
    /// A directory or a file this user cannot write claims nothing, so nothing is started.
    fn claim(&self) -> bool {
        let path = self.dir.join(LOCK_FILE);
        let last = |found: io::Result<fs::Metadata>| found.and_then(|file| file.modified()).ok();
        if !update::check_due(SystemTime::now(), last(fs::metadata(&path))) {
            return false;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).mode(FILE_MODE);
        match options.open(&path) {
            Ok(file) => {
                if file.try_lock().is_err() {
                    return false;
                }
                let now = SystemTime::now();
                update::check_due(now, last(file.metadata())) && file.set_modified(now).is_ok()
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                options.create_new(true).open(&path).is_ok()
            }
            Err(_) => false,
        }
    }

    /// The version installed: the lower of what the two names answer to `--version`, run with
    /// the caller's environment (`swarmops_ops::update::installed`).
    fn installed(&self) -> Option<Version> {
        let [swarmops, so] = NAMES.map(|name| {
            version_line(&self.dir.join(name), Probe::Inherited)
                .and_then(|line| Version::of_line(&line))
        });
        update::installed(swarmops, so)
    }

    /// Takes the lock one update holds, without waiting.
    fn lock(&self) -> Result<Lock, NoLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .open(self.dir.join(LOCK_FILE))
            .map_err(NoLock::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(Lock { file }),
            Err(TryLockError::WouldBlock) => Err(NoLock::Held),
            Err(TryLockError::Error(e)) => Err(NoLock::Io(e)),
        }
    }

    /// Takes the lock as [`Install::lock`] does, trying again for at most `wait` while it is
    /// held.
    fn lock_within(&self, wait: Duration) -> Result<Lock, NoLock> {
        let started = Instant::now();
        loop {
            match self.lock() {
                Err(NoLock::Held) if started.elapsed() < wait => thread::sleep(MAX_NAP / 5),
                taken => return taken,
            }
        }
    }

    /// Replaces the two binaries with `release`'s: staged by the installer, run once, then
    /// renamed over the old ones. The staging directory is removed before and after, however
    /// it ends. Until the first rename nothing is replaced.
    fn replace(
        &self,
        release: &Version,
        env: &[(&'static str, OsString)],
        deadline: Duration,
    ) -> Result<(), OpError> {
        let staging = self.dir.join(STAGING_DIR);
        clear(&staging).map_err(|e| {
            let why = format!("cannot remove what an earlier update left: {e}");
            failed_step("install", &why).with_details(path_details("install", &staging))
        })?;
        let replaced = stage(release, &staging, env, deadline)
            .and_then(|()| verify(release, &staging))
            .and_then(|()| self.swap(&staging));
        let _ = clear(&staging);
        replaced
    }

    /// Renames the staged binaries over the installed ones, `swarmops` first, and flushes the
    /// directory. A failure after the first rename leaves `swarmops` new and `so` old, which
    /// the next update repairs: the install is at the lower version.
    fn swap(&self, staging: &Path) -> Result<(), OpError> {
        for (done, name) in NAMES.iter().enumerate() {
            let installed = self.dir.join(name);
            fs::rename(staging.join(name), &installed).map_err(|e| {
                let state = if done == 0 {
                    "nothing was replaced"
                } else {
                    "swarmops was replaced and so was not: run update again"
                };
                let why = format!("cannot replace {}: {e}; {state}", installed.display());
                failed_step("swap", &why).with_details(path_details("swap", &installed))
            })?;
        }
        if let Ok(dir) = File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

/// Runs the installer for `release` into `staging`, with `env` and no other variable, for at
/// most `deadline`. `E_IO` (`details.step` `install`) when it cannot run, is stopped, or fails:
/// with its exit code and the last line it said.
fn stage(
    release: &Version,
    staging: &Path,
    env: &[(&'static str, OsString)],
    deadline: Duration,
) -> Result<(), OpError> {
    let mut installer = Command::new(SHELL);
    installer
        .args(["-s", "--", "--version"])
        .arg(release.to_string())
        .arg("--dir")
        .arg(staging)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)));
    let bounds = Bounds {
        deadline,
        kill_after: KILL_AFTER,
    };
    let ran = run_bounded(&mut installer, INSTALLER, bounds).map_err(|e| {
        let why = format!("cannot run the installer under {SHELL}: {e}; nothing was replaced");
        failed_step("install", &why)
    })?;
    let Some(status) = ran.status else {
        let why = format!(
            "the installer was stopped after {} seconds; nothing was replaced",
            deadline.as_secs()
        );
        let mut details = step_details("install");
        details.insert("deadline_s".to_owned(), deadline.as_secs().into());
        return Err(failed_step("install", &why).with_details(details));
    };
    if status.success() {
        return Ok(());
    }
    let said = last_line(&ran.stderr);
    let why = format!("the installer did not install {release} ({said}); nothing was replaced");
    let mut details = step_details("install");
    details.insert("installer_exit".to_owned(), status.code().into());
    details.insert("installer".to_owned(), said.into());
    Err(failed_step("install", &why).with_details(details))
}

/// Checks that each staged binary of `staging` is `release`'s: run once, with no `SWARMOPS_*`
/// variable, it prints exactly the line a release prints. `E_IO` (`details.step` `verify`,
/// `details.expected`, `details.found`) otherwise: a binary that does not start on this host
/// replaces nothing.
fn verify(release: &Version, staging: &Path) -> Result<(), OpError> {
    let expected = format!("swarmops {release}");
    for name in NAMES {
        let line = version_line(&staging.join(name), Probe::Clean);
        if line.as_deref() == Some(format!("{expected}\n").as_str()) {
            continue;
        }
        let found = line.map_or_else(
            || "nothing: it did not run".to_owned(),
            |line| output::printable(&line.chars().take(MAX_QUOTED_CHARS).collect::<String>()),
        );
        let why = format!(
            "the downloaded {name} does not print `{expected}` for --version; nothing was \
             replaced"
        );
        let mut details = step_details("verify");
        details.insert("expected".to_owned(), expected.into());
        details.insert("found".to_owned(), found.into());
        return Err(failed_step("verify", &why).with_details(details));
    }
    Ok(())
}

/// The lock one update holds, on the file that also keeps what automatic update remembers. The
/// system frees it when the file is closed, however the process ends.
struct Lock {
    /// The lock file, locked.
    file: File,
}

impl Lock {
    /// What the file holds; nothing where it cannot be read.
    fn state(&mut self) -> State {
        let mut text = String::new();
        let most = u64::try_from(MAX_KEPT_BYTES).unwrap_or(u64::MAX);
        let read = self
            .file
            .seek(SeekFrom::Start(0))
            .and_then(|_| (&self.file).take(most).read_to_string(&mut text));
        match read {
            Ok(_) => State::parse(&text),
            Err(_) => State::default(),
        }
    }

    /// Writes `state` as all the file holds, in place: another file renamed over it would be
    /// another lock.
    fn write(&mut self, state: &State) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(state.text().as_bytes())
    }
}

/// Why the lock was not taken.
enum NoLock {
    /// Another update holds it.
    Held,
    /// The file could not be opened or locked.
    Io(io::Error),
}

impl NoLock {
    /// The refusal of an `update` the user ran: `E_BUSY` (`details.reason` `update_running`)
    /// for a lock that is held, `E_IO` (`details.step` `lock`) for a file that cannot be made.
    fn refusal(self, install: &Install) -> OpError {
        match self {
            NoLock::Held => {
                let why = "another update of this install is running";
                let mut details = Map::new();
                details.insert("reason".to_owned(), "update_running".into());
                OpError::new(ErrorCode::Busy, why)
                    .with_details(details)
                    .with_hint("run it again once that one has ended")
            }
            NoLock::Io(e) => {
                let path = install.dir.join(LOCK_FILE);
                let why = format!(
                    "cannot take the update's lock, {}: {e}; nothing was replaced",
                    path.display()
                );
                failed_step("lock", &why)
                    .with_details(path_details("lock", &path))
                    .with_hint("update as the user who owns the install directory")
            }
        }
    }
}

/// How a program is run for its version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// With the caller's environment.
    Inherited,
    /// Without any `SWARMOPS_*` variable.
    Clean,
}

/// What `program --version` prints, when it ends well within [`PROBE_DEADLINE`] and prints
/// UTF-8. `None` for a program that is missing, fails, or says anything else.
fn version_line(program: &Path, probe: Probe) -> Option<String> {
    let mut command = Command::new(program);
    command.arg("--version");
    if probe == Probe::Clean {
        let own = std::env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| name.to_string_lossy().starts_with("SWARMOPS_"));
        for name in own {
            command.env_remove(name);
        }
    }
    let bounds = Bounds {
        deadline: PROBE_DEADLINE,
        kill_after: KILL_AFTER,
    };
    let ran = run_bounded(&mut command, "", bounds).ok()?;
    if !ran.status?.success() {
        return None;
    }
    String::from_utf8(ran.stdout).ok()
}

/// How long a child may run.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    /// When it is asked to end (SIGTERM, to its whole process group).
    deadline: Duration,
    /// How long after that it is killed.
    kill_after: Duration,
}

/// A child that was run to its end.
struct Ran {
    /// How it ended; `None` when it was stopped at its deadline.
    status: Option<ExitStatus>,
    /// The first [`MAX_KEPT_BYTES`] it printed on stdout.
    stdout: Vec<u8>,
    /// The last [`MAX_KEPT_BYTES`] it printed on stderr.
    stderr: Vec<u8>,
}

/// Runs `command` to its end within `bounds`, in a process group of its own, with `input` on
/// its stdin and both its outputs read as it writes them, so that it never waits on a full
/// pipe.
fn run_bounded(command: &mut Command, input: &'static str, bounds: Bounds) -> io::Result<Ran> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A child that ends before it has read its input closes the pipe: no failure of ours.
        thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let stdout = drain(child.stdout.take(), Keep::Head);
    let stderr = drain(child.stderr.take(), Keep::Tail);
    let status = wait(&mut child, bounds)?;
    let printed = |pipe: Receiver<Vec<u8>>| pipe.recv_timeout(DRAIN_WAIT).unwrap_or_default();
    Ok(Ran {
        status,
        stdout: printed(stdout),
        stderr: printed(stderr),
    })
}

/// Waits for `child`, which leads its process group, within `bounds`: at the deadline the group
/// is asked to end, and killed if it has not. `None` for a child stopped so.
fn wait(child: &mut Child, bounds: Bounds) -> io::Result<Option<ExitStatus>> {
    let started = Instant::now();
    let mut stopped: Option<Instant> = None;
    let mut nap = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(stopped.is_none().then_some(status)),
            Ok(None) => {}
            Err(e) => {
                end(child, Signal::KILL);
                return Err(e);
            }
        }
        match stopped {
            None if started.elapsed() >= bounds.deadline => {
                end(child, Signal::TERM);
                stopped = Some(Instant::now());
            }
            Some(at) if at.elapsed() >= bounds.kill_after => {
                end(child, Signal::KILL);
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            _ => {}
        }
        thread::sleep(nap);
        nap = (nap * 2).min(MAX_NAP);
    }
}

/// Sends `signal` to the process group `child` leads. The child is not reaped yet, so the
/// group's id is still its own.
fn end(child: &Child, signal: Signal) {
    let _ = rustix::process::kill_process_group(Pid::from_child(child), signal);
}

/// Which part of a child's output is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// The first [`MAX_KEPT_BYTES`].
    Head,
    /// The last [`MAX_KEPT_BYTES`].
    Tail,
}

/// Reads `pipe` to its end on a thread of its own, and gives what it kept when the pipe closes.
fn drain(pipe: Option<impl Read + Send + 'static>, keep: Keep) -> Receiver<Vec<u8>> {
    let (done, kept) = mpsc::channel();
    let Some(mut pipe) = pipe else {
        let _ = done.send(Vec::new());
        return kept;
    };
    thread::spawn(move || {
        let mut all = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => match keep {
                    Keep::Head => {
                        let room = MAX_KEPT_BYTES.saturating_sub(all.len());
                        all.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    Keep::Tail => {
                        all.extend_from_slice(&chunk[..n]);
                        if all.len() > 2 * MAX_KEPT_BYTES {
                            all.drain(..all.len() - MAX_KEPT_BYTES);
                        }
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if keep == Keep::Tail && all.len() > MAX_KEPT_BYTES {
            all.drain(..all.len() - MAX_KEPT_BYTES);
        }
        let _ = done.send(all);
    });
    kept
}

/// The last line the installer said, without its prefix; else the last line of anything it
/// printed on stderr; made printable, and cut to [`MAX_QUOTED_CHARS`].
fn last_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines = || text.lines().rev().filter(|line| !line.trim().is_empty());
    let said = lines()
        .find_map(|line| line.strip_prefix(INSTALLER_PREFIX))
        .or_else(|| lines().next())
        .unwrap_or("it said nothing");
    let cut: String = said.trim().chars().take(MAX_QUOTED_CHARS).collect();
    output::printable(&cut)
}

/// What the file at `path` holds, at most [`MAX_KEPT_BYTES`] of UTF-8. `None` when there is no
/// such file; `InvalidData` for one that is longer or is not UTF-8.
fn read_bounded(path: &Path) -> io::Result<Option<String>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let most = u64::try_from(MAX_KEPT_BYTES).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    file.take(most + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_KEPT_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::ErrorKind::InvalidData.into())
}

/// Writes `text` as the file at `path`, whole: to a temporary file beside it, flushed, then
/// renamed over it, and the directory flushed. A reader finds the old file or the new one.
fn replace_file(path: &Path, text: &str) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{}", std::process::id()));
    let staged = path.with_file_name(name);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(&staged)
        .and_then(|mut file| {
            // The mode whatever the caller's umask is, as the installer's chmod has it.
            file.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&staged, path));
    if written.is_err() {
        let _ = fs::remove_file(&staged);
        return written;
    }
    if let Some(dir) = path.parent().and_then(|dir| File::open(dir).ok()) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Removes `path` and everything in it, if there is anything: a directory with what it holds,
/// a file, or a link, which is not followed.
fn clear(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(found) if found.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// What the request for the latest release got instead of it.
#[derive(Debug, Clone, Copy)]
enum Unreached {
    /// curl reached no answer; its exit code, `None` when a signal ended it.
    Curl(Option<i32>),
    /// An answer that names no release: its HTTP status, when it had one.
    Answer(Option<u16>),
}

/// `E_REMOTE_UNREACHABLE` for a releases page that did not say what the latest release is:
/// `details.phase` `update`, `details.host`, and `details.curl_exit` with `details.cause`, or
/// `details.http_status`.
fn unreachable(what: Unreached) -> OpError {
    let host = host();
    let mut details = Map::new();
    details.insert("phase".to_owned(), "update".into());
    details.insert("host".to_owned(), host.into());
    let why = match what {
        Unreached::Curl(exit) => {
            let cause = device_flow::cause(exit);
            details.insert("cause".to_owned(), cause.into());
            if let Some(exit) = exit {
                details.insert("curl_exit".to_owned(), exit.into());
            }
            format!("{host} did not answer ({cause})")
        }
        Unreached::Answer(status) => {
            if let Some(status) = status {
                details.insert("http_status".to_owned(), status.into());
            }
            format!(
                "{host} did not name the latest release: its answer is no redirect to a \
                     release's page"
            )
        }
    };
    OpError::new(ErrorCode::RemoteUnreachable, why)
        .with_details(details)
        .with_hint("run it again once the network is back: nothing was replaced")
}

/// The host of the releases page.
fn host() -> &'static str {
    let rest = RELEASES_URL
        .strip_prefix("https://")
        .unwrap_or(RELEASES_URL);
    rest.split('/').next().unwrap_or(rest)
}

/// `E_INVALID_ARG` with `details.reason`.
fn refused(reason: &str, why: &str) -> OpError {
    let mut details = Map::new();
    details.insert("reason".to_owned(), reason.into());
    OpError::new(ErrorCode::InvalidArg, why).with_details(details)
}

/// `E_IO` with `details.step`.
fn failed_step(step: &str, why: &str) -> OpError {
    OpError::new(ErrorCode::Io, why).with_details(step_details(step))
}

/// Details holding `details.step` alone.
fn step_details(step: &str) -> Map<String, Value> {
    let mut details = Map::new();
    details.insert("step".to_owned(), step.into());
    details
}

/// Details holding `details.step` and `details.path`.
fn path_details(step: &str, path: &Path) -> Map<String, Value> {
    let mut details = step_details(step);
    details.insert("path".to_owned(), path.display().to_string().into());
    details
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bounds no test's child reaches.
    const WIDE: Bounds = Bounds {
        deadline: Duration::from_secs(60),
        kill_after: Duration::from_secs(5),
    };

    /// `/bin/sh -s`, which runs the script on its stdin.
    fn shell() -> Command {
        let mut shell = Command::new(SHELL);
        shell.arg("-s");
        shell
    }

    #[test]
    fn the_flags_name_one_form() {
        assert_eq!(Form::of(false, None, false, false), Form::Install);
        assert_eq!(Form::of(true, None, false, false), Form::Check);
        assert_eq!(Form::of(false, None, true, false), Form::Status);
        assert_eq!(Form::of(false, None, false, true), Form::Background);
        assert_eq!(
            Form::of(false, Some(AutoArg::On), false, false),
            Form::Auto(true)
        );
        assert_eq!(
            Form::of(false, Some(AutoArg::Off), false, false),
            Form::Auto(false)
        );
    }

    #[test]
    fn a_child_s_end_and_both_its_outputs_are_read() {
        let script = "echo out; echo first >&2; echo last >&2; exit 3\n";
        let ran = run_bounded(&mut shell(), script, WIDE).expect("run a shell");
        assert_eq!(ran.status.and_then(|status| status.code()), Some(3));
        assert_eq!(ran.stdout, b"out\n");
        assert_eq!(ran.stderr, b"first\nlast\n");

        // More than a pipe holds, on both: the child is never left waiting, the start of
        // stdout is kept, and the end of stderr.
        let script = "i=0; while [ $i -lt 4000 ]; do \
                      echo \"out $i 0123456789012345678901234567890123456789\"; \
                      echo \"err $i 0123456789012345678901234567890123456789\" >&2; \
                      i=$((i + 1)); done\n";
        let ran = run_bounded(&mut shell(), script, WIDE).expect("run a shell");
        assert_eq!(ran.status.and_then(|status| status.code()), Some(0));
        assert_eq!(ran.stdout.len(), MAX_KEPT_BYTES);
        assert!(ran.stdout.starts_with(b"out 0 "), "the start of stdout");
        assert_eq!(ran.stderr.len(), MAX_KEPT_BYTES);
        assert!(
            ran.stderr
                .ends_with(b"err 3999 0123456789012345678901234567890123456789\n"),
            "the end of stderr"
        );

        // A program that is not there is an error, not a run.
        let mut missing = Command::new("/nonexistent/swarmops-test-program");
        assert!(run_bounded(&mut missing, "", WIDE).is_err());
    }

    #[test]
    fn a_child_past_its_deadline_is_stopped_with_its_group() {
        // 1. Asked to end, it ends: the shell and the sleep it waits for, at once.
        let bounds = Bounds {
            deadline: Duration::from_millis(200),
            kill_after: Duration::from_secs(30),
        };
        let started = Instant::now();
        let ran =
            run_bounded(&mut shell(), "echo begun >&2; sleep 60\n", bounds).expect("run a shell");
        assert!(ran.status.is_none(), "stopped at its deadline");
        assert_eq!(ran.stderr, b"begun\n");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "not waited for"
        );

        // 2. One that ignores the request is killed, and its group with it.
        let bounds = Bounds {
            deadline: Duration::from_millis(200),
            kill_after: Duration::from_millis(300),
        };
        let started = Instant::now();
        let ran =
            run_bounded(&mut shell(), "trap '' TERM; sleep 60\n", bounds).expect("run a shell");
        assert!(ran.status.is_none(), "killed after its deadline");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "not waited for"
        );
    }

    #[test]
    fn a_version_is_asked_of_a_program_that_answers() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let program = |name: &str, script: &str| {
            let path = dir.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write a script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("set its mode");
            path
        };

        let answers = program("answers", "echo \"swarmops 1.2.3\"");
        let line = version_line(&answers, Probe::Inherited);
        assert_eq!(line.as_deref(), Some("swarmops 1.2.3\n"));
        let version = line.and_then(|line| Version::of_line(&line));
        assert_eq!(version, Version::parse("1.2.3"));

        // A program that fails, is missing, or is no program has no version.
        let fails = program("fails", "echo \"swarmops 1.2.3\"; exit 1");
        assert_eq!(version_line(&fails, Probe::Inherited), None);
        assert_eq!(
            version_line(&dir.path().join("missing"), Probe::Inherited),
            None
        );
        assert_eq!(version_line(dir.path(), Probe::Inherited), None);

        // It is asked for `--version`, and a clean run has no SWARMOPS_* variable: this
        // process's own, if it has any, are not passed on.
        let echoes = program("echoes", "echo \"$1\"; /usr/bin/env");
        let printed = version_line(&echoes, Probe::Clean).expect("it ran");
        assert!(printed.starts_with("--version\n"), "{printed}");
        assert!(
            !printed.lines().any(|line| line.starts_with("SWARMOPS_")),
            "{printed}"
        );
    }

    #[test]
    fn the_installer_s_last_line_is_what_it_said() {
        let said = "swarmops-install: Installing SwarmOps 1.2.3 (x) into /d.\n\
                    swarmops-install: x.tar.gz does not match its line in SHA256SUMS: nothing \
                    was installed.\n";
        assert_eq!(
            last_line(said.as_bytes()),
            "x.tar.gz does not match its line in SHA256SUMS: nothing was installed."
        );
        // A tool's own complaint after it is not the installer's line.
        let with_noise = format!("{said}curl: (22) error\n\n");
        assert_eq!(
            last_line(with_noise.as_bytes()),
            "x.tar.gz does not match its line in SHA256SUMS: nothing was installed."
        );
        // Without a line of the installer's, the last line of anything.
        assert_eq!(
            last_line(b"sh: 1: uname: not found\n"),
            "sh: 1: uname: not found"
        );
        assert_eq!(last_line(b""), "it said nothing");
        assert_eq!(last_line(b"\n  \n"), "it said nothing");
        // No control character reaches an envelope or a terminal, and no line is long.
        let hostile = format!("swarmops-install: a\u{1b}[2Jb {}\n", "x".repeat(500));
        let line = last_line(hostile.as_bytes());
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(line.chars().count() <= MAX_QUOTED_CHARS + 16, "{line:?}");
    }

    #[test]
    fn the_installer_gets_no_variable_of_swarmops_own() {
        let env = |name: &str| Some(OsString::from(format!("value of {name}")));
        let kept = installer_env(&env);
        let names: Vec<&str> = kept.iter().map(|(name, _)| *name).collect();
        for name in ["PATH", "HOME", "TMPDIR", "HTTPS_PROXY", "CURL_CA_BUNDLE"] {
            assert!(names.contains(&name), "{name} is kept: {names:?}");
        }
        assert!(
            !names.iter().any(|name| name.starts_with("SWARMOPS_")),
            "{names:?}"
        );
        assert_eq!(names.len(), CURL_ENV_KEPT.len() + INSTALLER_ENV_KEPT.len());
        // Only what the caller has set.
        assert_eq!(installer_env(&|_| None), []);

        // The script is the repository's, whole: its last line runs it.
        assert!(INSTALLER.starts_with("#!/bin/sh\n"));
        assert!(INSTALLER.ends_with("\nmain \"$@\"\n"));
        assert!(INSTALLER.contains(&format!("PREFIX='{}'", INSTALLER_PREFIX.trim_end())));
    }

    #[test]
    fn a_file_beside_the_binaries_is_read_whole_or_not_at_all() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("file");
        assert_eq!(read_bounded(&path).expect("no file"), None);

        replace_file(&path, "one\n").expect("write it");
        assert_eq!(read_bounded(&path).expect("read").as_deref(), Some("one\n"));
        replace_file(&path, "two\n").expect("write it again");
        assert_eq!(read_bounded(&path).expect("read").as_deref(), Some("two\n"));
        let mode = fs::metadata(&path).expect("the file").mode() & 0o7777;
        assert_eq!(mode, FILE_MODE, "the installer's mode");
        let names: Vec<_> = fs::read_dir(dir.path())
            .expect("the directory")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(names, ["file"], "no temporary file is left");

        fs::write(&path, vec![b'a'; MAX_KEPT_BYTES]).expect("a file at the bound");
        assert!(read_bounded(&path).expect("read").is_some());
        fs::write(&path, vec![b'a'; MAX_KEPT_BYTES + 1]).expect("a file over the bound");
        let long = read_bounded(&path).expect_err("too long");
        assert_eq!(long.kind(), io::ErrorKind::InvalidData);
        fs::write(&path, b"\xff\xfe").expect("a file that is not UTF-8");
        let binary = read_bounded(&path).expect_err("not UTF-8");
        assert_eq!(binary.kind(), io::ErrorKind::InvalidData);

        // A directory that cannot be written leaves the old file, and no temporary one.
        let missing = dir.path().join("missing").join("file");
        assert!(replace_file(&missing, "x").is_err());
    }

    #[test]
    fn what_an_earlier_update_left_is_removed_without_following_a_link() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let staging = dir.path().join(STAGING_DIR);
        clear(&staging).expect("nothing to remove");

        fs::create_dir_all(staging.join("sub")).expect("a directory");
        fs::write(staging.join("sub").join("swarmops"), "x").expect("a file in it");
        clear(&staging).expect("remove a directory");
        assert!(!staging.exists());

        fs::write(&staging, "x").expect("a file");
        clear(&staging).expect("remove a file");
        assert!(!staging.exists());

        let kept = dir.path().join("kept");
        fs::create_dir(&kept).expect("a directory");
        fs::write(kept.join("file"), "x").expect("a file in it");
        std::os::unix::fs::symlink(&kept, &staging).expect("a link");
        clear(&staging).expect("remove a link");
        assert!(fs::symlink_metadata(&staging).is_err(), "the link is gone");
        assert!(kept.join("file").is_file(), "what it named is not");
    }

    #[test]
    fn one_update_holds_the_lock_and_the_file_keeps_the_state() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let install = Install {
            dir: dir.path().to_path_buf(),
        };
        let release = Version::parse("1.2.3").expect("a version");

        let mut lock = install.lock().ok().expect("the first takes it");
        assert!(matches!(install.lock(), Err(NoLock::Held)));
        assert_eq!(lock.state(), State::default());

        let mut state = State::default();
        state.record_failure(&release);
        lock.write(&state).expect("write the state");
        assert_eq!(lock.state(), state);
        // A shorter state leaves nothing of the longer one.
        assert!(state.forget_failure());
        lock.write(&state).expect("write the state");
        assert_eq!(lock.state(), State::default());
        state.record_failure(&release);
        lock.write(&state).expect("write the state");

        // Held, an update the user ran is refused as busy; freed, the next one reads the state.
        let Err(held) = install.lock() else {
            panic!("the lock is held");
        };
        let busy = held.refusal(&install);
        assert_eq!(busy.code, ErrorCode::Busy.as_str());
        assert_eq!(busy.details["reason"], "update_running");
        drop(lock);
        let mut next = install.lock().ok().expect("free again");
        assert!(next.state().has_failed(&release));

        // A directory that cannot hold the file: E_IO, and where.
        let nowhere = Install {
            dir: dir.path().join("missing"),
        };
        let Err(failed) = nowhere.lock() else {
            panic!("no directory, no lock");
        };
        let error = failed.refusal(&nowhere);
        assert_eq!(error.code, ErrorCode::Io.as_str());
        assert_eq!(error.details["step"], "lock");
        assert!(error.details["path"].is_string());
    }

    /// The lock file of `install`, made if there is none, as last claimed `ago` before now, or
    /// that long after it.
    fn claimed(install: &Install, ago: Duration, future: bool) -> SystemTime {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(install.dir.join(LOCK_FILE))
            .expect("the lock file");
        let now = SystemTime::now();
        let at = if future { now + ago } else { now - ago };
        file.set_modified(at).expect("set its time");
        at
    }

    /// When the lock file of `install` was last claimed.
    fn last_claim(install: &Install) -> SystemTime {
        fs::metadata(install.dir.join(LOCK_FILE))
            .and_then(|file| file.modified())
            .expect("the lock file's time")
    }

    #[test]
    fn a_check_is_claimed_once_a_day_by_one_command() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let install = Install {
            dir: dir.path().to_path_buf(),
        };
        let hour = Duration::from_hours(1);
        // Whether the file's time is that of a claim made since `before`.
        let claimed_since = |before: SystemTime| {
            let at = last_claim(&install);
            before <= at && at <= SystemTime::now()
        };

        // 1. No lock file: the command that makes it has the claim, and the next has none.
        assert!(install.claim());
        assert!(install.dir.join(LOCK_FILE).is_file());
        assert!(!install.claim());

        // 2. Claimed less than a day ago: nothing, and the file's time stays.
        let at = claimed(&install, 23 * hour, false);
        assert!(!install.claim());
        assert_eq!(last_claim(&install), at);

        // 3. A day ago or more: claimed, once, and the time is the claim's.
        claimed(&install, 25 * hour, false);
        let before = SystemTime::now();
        assert!(install.claim());
        assert!(claimed_since(before));
        assert!(!install.claim());

        // 4. A time in the future, as a clock set back leaves: long ago.
        claimed(&install, 48 * hour, true);
        let before = SystemTime::now();
        assert!(install.claim());
        assert!(claimed_since(before));

        // 5. Due, but an update holds the lock: nothing, and the time stays.
        let at = claimed(&install, 25 * hour, false);
        let lock = install.lock().ok().expect("take the lock");
        assert!(!install.claim());
        assert_eq!(last_claim(&install), at);
        drop(lock);

        // 6. What the file remembers is kept by a claim.
        let mut lock = install.lock().ok().expect("take the lock");
        let mut state = State::default();
        state.record_failure(&Version::parse("1.2.3").expect("a version"));
        lock.write(&state).expect("write the state");
        drop(lock);
        claimed(&install, 25 * hour, false);
        assert!(install.claim());
        let mut lock = install.lock().ok().expect("take the lock");
        assert_eq!(lock.state(), state);
        drop(lock);

        // 7. Nowhere to claim: a directory that is not there, and one that cannot be written.
        let nowhere = Install {
            dir: dir.path().join("missing"),
        };
        assert!(!nowhere.claim());
        if !rustix::process::geteuid().is_root() {
            let closed = Install {
                dir: dir.path().join("closed"),
            };
            fs::create_dir(&closed.dir).expect("a directory");
            let mode = |mode| fs::set_permissions(&closed.dir, fs::Permissions::from_mode(mode));
            mode(0o555).expect("close the directory");
            assert!(!closed.claim());
            mode(0o755).expect("open the directory");
            assert!(!closed.dir.join(LOCK_FILE).exists());
        }
    }

    /// Of many commands that find a check due at once, one has the claim: whether the lock file
    /// is there, old, or has yet to be made.
    #[test]
    fn of_commands_at_once_one_claims_the_check() {
        const COMMANDS: usize = 32;
        for made in [false, true] {
            let dir = tempfile::tempdir().expect("a temp dir");
            let install = Install {
                dir: dir.path().to_path_buf(),
            };
            if made {
                claimed(&install, Duration::from_hours(25), false);
            }
            let start = std::sync::Barrier::new(COMMANDS);
            let claims = thread::scope(|scope| {
                let commands: Vec<_> = (0..COMMANDS)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            install.claim()
                        })
                    })
                    .collect();
                commands
                    .into_iter()
                    .map(|command| command.join().expect("a command"))
                    .filter(|claimed| *claimed)
                    .count()
            });
            assert_eq!(claims, 1, "lock file made before: {made}");
        }
    }

    #[test]
    fn a_note_is_printed_once_and_removed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let install = Install {
            dir: dir.path().to_path_buf(),
        };
        let path = install.dir.join(NOTE_FILE);

        // No note, nothing said.
        let mut log = Vec::new();
        install.say_note(&mut log);
        assert_eq!(log, b"");

        // A note: its sentence on one line, and the file is gone, so the next says nothing.
        let note = Note::parse("updated 1.2.3 1.3.0\n").expect("a note");
        install.leave(&note).expect("leave the note");
        install.say_note(&mut log);
        let said = String::from_utf8(log).expect("UTF-8");
        assert_eq!(said, format!("{}\n", note.sentence()));
        assert!(said.starts_with("swarmops: updated from 1.2.3 to 1.3.0. "));
        assert!(!path.exists());
        let mut log = Vec::new();
        install.say_note(&mut log);
        assert_eq!(log, b"");

        // A file that holds no note is left, and says nothing.
        fs::write(&path, "nothing this build knows\n").expect("write a file");
        install.say_note(&mut log);
        assert_eq!(log, b"");
        assert!(path.exists());
    }

    /// The line the run in the background is started by gives the program the two words, and
    /// none of the descriptors 3 to 9 its parent had.
    #[test]
    fn the_background_run_is_started_without_its_parent_s_descriptors() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let seen = dir.path().join("seen");
        let probe = dir.path().join("a probe");
        let script = "#!/bin/sh\n\
                      out=$1; shift\n\
                      open=\n\
                      for n in 3 4 5 6 7 8 9; do\n\
                      \x20 if (eval \": >&$n\") 2>/dev/null; then open=\"$open $n\"; fi\n\
                      done\n\
                      echo \"$*:$open\" > \"$out\"\n";
        fs::write(&probe, script).expect("write the probe");
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).expect("set its mode");

        // 1. The probe sees what it is given: the control.
        let held = "3>/dev/null 5>/dev/null 9>/dev/null";
        let line = format!("\"$0\" \"$1\" update --background {held}");
        let status = Command::new(SHELL)
            .args(["-c", &line])
            .args([&probe, &seen])
            .status()
            .expect("run a shell");
        assert!(status.success());
        let open = fs::read_to_string(&seen).expect("what the probe saw");
        assert_eq!(open, "update --background: 3 5 9\n");

        // 2. Started by the line, from a shell that holds the same three, it has none. The
        //    line gives its program no argument to say where to write, so a wrapper does.
        assert!(!DETACHED.contains('\''), "it is quoted whole below");
        let wrapper = dir.path().join("wrapper");
        let starts_probe = format!(
            "#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n",
            probe.display(),
            seen.display()
        );
        fs::write(&wrapper, starts_probe).expect("write the wrapper");
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("set its mode");
        let line = format!("{SHELL} -c '{DETACHED}' \"$0\" {held}");
        let status = Command::new(SHELL)
            .args(["-c", &line])
            .arg(&wrapper)
            .status()
            .expect("run a shell");
        assert!(status.success());
        let open = fs::read_to_string(&seen).expect("what the probe saw");
        assert_eq!(open, "update --background:\n");
    }

    #[test]
    fn a_releases_page_that_names_no_release_is_unreachable() {
        assert_eq!(host(), "github.com");

        let timeout = unreachable(Unreached::Curl(Some(28)));
        assert_eq!(timeout.code, ErrorCode::RemoteUnreachable.as_str());
        assert_eq!(timeout.details["phase"], "update");
        assert_eq!(timeout.details["host"], "github.com");
        assert_eq!(timeout.details["curl_exit"], 28);
        assert_eq!(timeout.details["cause"], "timeout");
        let killed = unreachable(Unreached::Curl(None));
        assert!(!killed.details.contains_key("curl_exit"));

        let portal = unreachable(Unreached::Answer(Some(200)));
        assert_eq!(portal.details["http_status"], 200);
        assert!(!portal.details.contains_key("curl_exit"));
        let unread = unreachable(Unreached::Answer(None));
        assert!(!unread.details.contains_key("http_status"));
    }

    #[test]
    fn a_note_s_reason_is_the_installer_s_line_or_the_error_s() {
        let mut details = step_details("install");
        details.insert("installer".to_owned(), "no macOS build".into());
        let install = failed_step("install", "the installer did not install").with_details(details);
        assert_eq!(why(&install), "no macOS build");
        let verify = failed_step("verify", "the downloaded so does not print");
        assert_eq!(why(&verify), "the downloaded so does not print");
    }

    /// This test binary is no release's build and has no receipt beside it: the forms that
    /// need neither answer, the others are refused before any file or request.
    #[test]
    fn a_build_from_source_says_so_and_replaces_nothing() {
        if version::is_release() {
            return;
        }
        let env = |_: &str| None;
        let status = update(&env, Form::Status).expect("status");
        assert_eq!(status.auto_update, AutoUpdate::OffSourceBuild);
        assert_eq!(status.current, version::version());
        assert!(!status.updated && status.dir.is_none() && status.pending.is_none());
        assert_eq!(update(&env, Form::Auto(false)).expect("off"), status);
        assert_eq!(update(&env, Form::Background).expect("background"), status);

        for form in [Form::Install, Form::Check, Form::Auto(true)] {
            let refused = update(&env, form).expect_err("no release's build");
            assert_eq!(refused.code, ErrorCode::InvalidArg.as_str(), "{form:?}");
            assert_eq!(refused.details["reason"], "not_a_release_build", "{form:?}");
            assert!(refused.hint.is_some(), "{form:?}");
        }
    }
}
