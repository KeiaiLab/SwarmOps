//! The program's own update, as far as it is decided without touching a file or the network:
//! what `update` and automatic update compare, read, write and decide. The CLI reads the files
//! and the environment, runs curl and the installer, and asks here what each answer means.
//!
//! # Versions
//!
//! A [`Version`] is three numbers and, after a hyphen, a pre-release, the installer's rule for
//! `--version`: `0.12.0`, `1.0.0-rc.1`. Releases order by their numbers. A pre-release orders
//! before its release and after every earlier release; two different pre-releases of one
//! release are not ordered, so neither is newer than the other and neither replaces the other.
//!
//! # The files beside the binaries
//!
//! Everything an update keeps is in the directory the binaries are installed in, so that it
//! belongs to one install and to no user's state directory:
//!
//! | File | Holds |
//! | --- | --- |
//! | [`RECEIPT_FILE`] | The installer's receipt, TOML: `receipt = 1` and `auto_update = true` or `false`. |
//! | [`LOCK_FILE`] | The lock one update holds, and [`State`]: a line each for the release last seen unmarked, announced and failed. Its modification time is when a check was last claimed. |
//! | [`NOTE_FILE`] | One line, a [`Note`]: what the next command at a terminal says once. |
//! | [`STAGING_DIR`] | The directory a new release is unpacked and tried in before it replaces anything. |
//!
//! A [`Receipt`] is read whole and refused when it is not one ([`ReceiptFault`]); a rewrite
//! changes `auto_update` and keeps every key this build does not know. Comments are not kept.
//! The lock file's lines and the note are this program's own and are read leniently: a line
//! that says nothing known is kept and means nothing, and a note that is none is no note.
//!
//! # Whether an install updates itself
//!
//! [`auto_update`] decides from [`Facts`], the first of these that holds:
//!
//! | Order | Fact | Answer |
//! | --- | --- | --- |
//! | 1 | The build is no release's | `off_source_build` |
//! | 2 | No receipt is beside the binaries | `off_no_receipt` |
//! | 3 | The receipt says `auto_update = false` | `off` |
//! | 4 | [`AUTO_UPDATE_ENV`] is `off`, or neither `on` nor `off` | `off_env` |
//! | 5 | [`CI_ENV`] is set, and the variable is not `on` | `off_ci` |
//! | 6 | The host is a container, and the variable is not `on` | `off_container` |
//! | 7 | The caller is root, or the directory is another user's | `off_owner` |
//! | 8 | Otherwise | `on` |
//!
//! `on` in the variable cancels rules 5 and 6 and nothing else. An explicit `update` needs
//! only what rules 1 and 2 ask ([`eligible`]): the user asked.
//!
//! # What automatic update installs, and says
//!
//! Only the latest release is looked at ([`latest_address`], read from where it redirects to:
//! [`release_at`]). It is installed automatically when it carries the mark for the minor line
//! the running build is of ([`mark_address`]). A release without the mark is announced, and
//! only at its second sighting ([`State::sight_unmarked`]): a release is the latest before its
//! marks are uploaded. A release that failed to install is not tried again automatically
//! ([`State::record_failure`]).

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::time::{Duration, SystemTime};

use swarmops_api::{AutoUpdate, UpdatePending, UpdatePendingKind};

/// The installer's receipt, beside the binaries.
pub const RECEIPT_FILE: &str = ".swarmops-receipt";

/// The lock one update holds, which also keeps [`State`]'s lines, beside the binaries.
pub const LOCK_FILE: &str = ".swarmops-update.lock";

/// The note for the next command at a terminal, beside the binaries.
pub const NOTE_FILE: &str = ".swarmops-update.note";

/// The directory a new release is unpacked and tried in, beside the binaries.
pub const STAGING_DIR: &str = ".swarmops-update.d";

/// The environment variable that turns automatic update off (`off`), or on where `CI` or a
/// container would turn it off (`on`).
pub const AUTO_UPDATE_ENV: &str = "SWARMOPS_AUTO_UPDATE";

/// The environment variable most CI systems set.
pub const CI_ENV: &str = "CI";

/// The files whose presence says that the host is a container: Docker's and Podman's.
pub const CONTAINER_MARKERS: [&str; 2] = ["/.dockerenv", "/run/.containerenv"];

/// How long after a check the next one is due.
pub const CHECK_INTERVAL: Duration = Duration::from_hours(24);

/// Where the releases are: the page whose `latest` redirects to the latest release's.
pub const RELEASES_URL: &str = "https://github.com/KeiaiLab/SwarmOps/releases";

/// How the name of a release's mark starts: the file `auto-update-from-<major>.<minor>` says
/// that a build of that minor line may install the release automatically.
pub const MARK_PREFIX: &str = "auto-update-from-";

/// The one receipt version this build reads and writes.
const RECEIPT_VERSION: i64 = 1;

/// The receipt's keys.
const RECEIPT_KEY: &str = "receipt";
const AUTO_UPDATE_KEY: &str = "auto_update";

/// The words a line of the lock file starts with.
const SEEN: &str = "seen";
const ANNOUNCED: &str = "announced";
const FAILED: &str = "failed";

/// The words a note starts with.
const UPDATED: &str = "updated";
const AVAILABLE: &str = "available";

/// The most characters of the installer's last line a note keeps.
const MAX_WHY_CHARS: usize = 200;

/// A version of the program: three numbers, and a pre-release after a hyphen if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The three numbers: major, minor, patch.
    numbers: (u64, u64, u64),
    /// The pre-release, without its hyphen.
    pre: Option<String>,
}

impl Version {
    /// The version `text` is: three numbers between dots, and after a hyphen a pre-release of
    /// letters, digits, dots and hyphens. `None` for anything else, a leading `v` and semver's
    /// `+` build metadata included.
    pub fn parse(text: &str) -> Option<Version> {
        let (triple, pre) = match text.split_once('-') {
            Some((triple, pre)) => (triple, Some(pre)),
            None => (text, None),
        };
        let number = |part: &str| {
            let digits = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
            digits.then(|| part.parse::<u64>().ok()).flatten()
        };
        let mut parts = triple.split('.');
        let numbers = (
            number(parts.next()?)?,
            number(parts.next()?)?,
            number(parts.next()?)?,
        );
        if parts.next().is_some() {
            return None;
        }
        let label = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-');
        if pre.is_some_and(|pre| pre.is_empty() || !pre.bytes().all(label)) {
            return None;
        }
        Some(Version {
            numbers,
            pre: pre.map(str::to_owned),
        })
    }

    /// The version a `--version` line names: `swarmops <version>`, with or without the commit
    /// a build from a clone appends after a `+`, which says nothing of the order and is
    /// dropped. `None` for any other text: the answer of a program that is not SwarmOps'.
    pub fn of_line(line: &str) -> Option<Version> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let text = line.strip_prefix("swarmops ")?;
        let version = match text.split_once('+') {
            Some((version, commit)) => {
                let named = !commit.is_empty() && commit.bytes().all(|b| b.is_ascii_alphanumeric());
                named.then_some(version)?
            }
            None => text,
        };
        Version::parse(version)
    }

    /// Whether the version is a release's: it has no pre-release.
    pub fn is_release(&self) -> bool {
        self.pre.is_none()
    }

    /// The minor line the version is of, as a mark names it: `<major>.<minor>`.
    pub fn minor_line(&self) -> String {
        format!("{}.{}", self.numbers.0, self.numbers.1)
    }

    /// Whether this version may replace an install at `installed` ([`installed`]) when a
    /// program of version `running` asks: never when it is older than `running`, so that
    /// nothing installs an older version than the one that looked.
    ///
    /// - An install with a version is replaced by a strictly newer one.
    /// - An install with none, where a name is missing or answers as another program, is
    ///   replaced by `running`'s own version too: that writes the name again.
    ///
    /// A version in no order with one of the two replaces nothing.
    pub fn replaces(&self, installed: Option<&Version>, running: &Version) -> bool {
        self >= running && installed.is_none_or(|installed| self > installed)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (major, minor, patch) = self.numbers;
        write!(f, "{major}.{minor}.{patch}")?;
        match &self.pre {
            Some(pre) => write!(f, "-{pre}"),
            None => Ok(()),
        }
    }
}

/// By the numbers; a pre-release before its release; two different pre-releases of one release
/// in no order.
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        match self.numbers.cmp(&other.numbers) {
            Ordering::Equal => match (&self.pre, &other.pre) {
                (None, None) => Some(Ordering::Equal),
                (Some(_), None) => Some(Ordering::Less),
                (None, Some(_)) => Some(Ordering::Greater),
                (Some(mine), Some(theirs)) => (mine == theirs).then_some(Ordering::Equal),
            },
            unequal => Some(unequal),
        }
    }
}

/// The version an install is at, from what its two names answer to `--version`
/// ([`Version::of_line`]): the lower one, so that an install stopped between the two renames is
/// repaired by the next update. `None` when either name is missing or not SwarmOps': the
/// install has no version, and what may replace it is the running program's to bound
/// ([`Version::replaces`]). Of two pre-releases in no order it is `swarmops`'s: whatever is newer than
/// one of them is newer than the other.
pub fn installed(swarmops: Option<Version>, so: Option<Version>) -> Option<Version> {
    let (swarmops, so) = (swarmops?, so?);
    Some(if so < swarmops { so } else { swarmops })
}

/// The address whose redirect names the latest release.
pub fn latest_address() -> String {
    format!("{RELEASES_URL}/latest")
}

/// The page of `release`, which holds its notes: where [`latest_address`] redirects to for the
/// latest one.
pub fn notes_address(release: &Version) -> String {
    format!("{RELEASES_URL}/tag/v{release}")
}

/// The address of the mark that lets a build of `running`'s minor line install `release`
/// automatically. The release holds the file, or the address answers 404.
pub fn mark_address(release: &Version, running: &Version) -> String {
    format!(
        "{RELEASES_URL}/download/v{release}/{MARK_PREFIX}{}",
        running.minor_line()
    )
}

/// The release whose page `address` is, when it is exactly a release's [`notes_address`]:
/// this host, https, the tag of three plain numbers and nothing after it. `None` for anything
/// else a host may have answered, a pre-release's page included.
pub fn release_at(address: &str) -> Option<Version> {
    let tag = address.strip_prefix(RELEASES_URL)?.strip_prefix("/tag/v")?;
    let release = Version::parse(tag)?;
    (release.is_release() && notes_address(&release) == address).then_some(release)
}

/// Why a receipt's text is not one this build reads. No message quotes the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiptFault {
    /// The text is not TOML.
    NotToml,
    /// `receipt` is missing or not this build's version.
    Version,
    /// `auto_update` is missing or neither `true` nor `false`.
    AutoUpdate,
    /// The keys this build does not know could not be written back.
    Unwritable,
}

impl ReceiptFault {
    /// What is wrong, to end the sentence "the receipt …".
    pub fn why(self) -> &'static str {
        match self {
            ReceiptFault::NotToml => "is not TOML",
            ReceiptFault::Version => "names no `receipt = 1`",
            ReceiptFault::AutoUpdate => "names no `auto_update = true` or `false`",
            ReceiptFault::Unwritable => "holds keys that cannot be written back",
        }
    }
}

/// The installer's receipt: that the installer put the binaries there, and whether they may
/// update themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct Receipt {
    /// What `auto_update` says.
    auto_update: bool,
    /// Every other key of the file but `receipt`, as read.
    rest: toml::Table,
}

impl Receipt {
    /// The receipt `text` is, with the keys this build does not know kept as read.
    pub fn parse(text: &str) -> Result<Receipt, ReceiptFault> {
        let mut rest = text
            .parse::<toml::Table>()
            .map_err(|_| ReceiptFault::NotToml)?;
        if rest.remove(RECEIPT_KEY) != Some(toml::Value::Integer(RECEIPT_VERSION)) {
            return Err(ReceiptFault::Version);
        }
        let Some(toml::Value::Boolean(auto_update)) = rest.remove(AUTO_UPDATE_KEY) else {
            return Err(ReceiptFault::AutoUpdate);
        };
        Ok(Receipt { auto_update, rest })
    }

    /// Whether the receipt lets the binaries update themselves.
    pub fn auto_update(&self) -> bool {
        self.auto_update
    }

    /// Says whether the binaries may update themselves, as `update --auto on|off` does.
    pub fn set_auto_update(&mut self, on: bool) {
        self.auto_update = on;
    }

    /// The file's text: the installer's two lines, byte for byte what it writes, then every
    /// key this build does not know.
    pub fn text(&self) -> Result<String, ReceiptFault> {
        let mut text = format!(
            "{RECEIPT_KEY} = {RECEIPT_VERSION}\n{AUTO_UPDATE_KEY} = {}\n",
            self.auto_update
        );
        if !self.rest.is_empty() {
            text.push_str(&toml::to_string(&self.rest).map_err(|_| ReceiptFault::Unwritable)?);
        }
        Ok(text)
    }
}

/// What [`AUTO_UPDATE_ENV`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Switch {
    /// Unset, or set to the empty string.
    Unset,
    /// `on`: automatic update stays on in CI and in a container.
    On,
    /// `off`.
    Off,
    /// Any other value, one that is not UTF-8 included: it counts as `off`, so that a value
    /// mistyped by someone who wanted it off does not leave it on.
    Invalid,
}

impl Switch {
    /// What `value`, the variable's, says.
    pub fn of(value: Option<&OsStr>) -> Switch {
        match value.map(OsStr::as_encoded_bytes) {
            None | Some(b"") => Switch::Unset,
            Some(b"on") => Switch::On,
            Some(b"off") => Switch::Off,
            Some(_) => Switch::Invalid,
        }
    }

    /// What [`AUTO_UPDATE_ENV`] says in `env`.
    pub fn read(env: &dyn Fn(&str) -> Option<OsString>) -> Switch {
        Switch::of(env(AUTO_UPDATE_ENV).as_deref())
    }
}

/// Whether `value`, [`CI_ENV`]'s, says that a CI system runs the command: anything but unset,
/// empty, `0` and `false`.
pub fn ci_of(value: Option<&OsStr>) -> bool {
    !matches!(
        value.map(OsStr::as_encoded_bytes),
        None | Some(b"" | b"0" | b"false")
    )
}

/// Whether [`CI_ENV`] in `env` says that a CI system runs the command ([`ci_of`]).
pub fn in_ci(env: &dyn Fn(&str) -> Option<OsString>) -> bool {
    ci_of(env(CI_ENV).as_deref())
}

/// What [`auto_update`] decides from: the build, the receipt, the environment, the host, and
/// who runs the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Facts {
    /// Whether the build is a release's.
    pub release_build: bool,
    /// What the receipt beside the binaries says of `auto_update`; `None` when there is no
    /// receipt.
    pub receipt: Option<bool>,
    /// What [`AUTO_UPDATE_ENV`] says.
    pub switch: Switch,
    /// Whether [`CI_ENV`] is set ([`ci_of`]).
    pub ci: bool,
    /// Whether one of [`CONTAINER_MARKERS`] exists.
    pub container: bool,
    /// The effective user id of the caller.
    pub euid: u32,
    /// The user id that owns the directory the binaries are in.
    pub dir_owner: u32,
}

impl Facts {
    /// The facts of an install with a receipt that says `auto_update`, run by `euid` in a
    /// directory `dir_owner` owns, in a release's build, with nothing in the environment and
    /// outside a container. The other fields are public: set what differs.
    pub fn new(auto_update: bool, euid: u32, dir_owner: u32) -> Facts {
        Facts {
            release_build: true,
            receipt: Some(auto_update),
            switch: Switch::Unset,
            ci: false,
            container: false,
            euid,
            dir_owner,
        }
    }
}

/// Whether an install updates itself after a command: the first rule of the module's table
/// that holds. The parent's gate and `update --status` both ask here, so they cannot differ.
pub fn auto_update(facts: &Facts) -> AutoUpdate {
    if !facts.release_build {
        return AutoUpdate::OffSourceBuild;
    }
    match facts.receipt {
        None => return AutoUpdate::OffNoReceipt,
        Some(false) => return AutoUpdate::Off,
        Some(true) => {}
    }
    let forced_on = match facts.switch {
        Switch::Off | Switch::Invalid => return AutoUpdate::OffEnv,
        Switch::On => true,
        Switch::Unset => false,
    };
    if facts.ci && !forced_on {
        return AutoUpdate::OffCi;
    }
    if facts.container && !forced_on {
        return AutoUpdate::OffContainer;
    }
    if facts.euid == 0 || facts.euid != facts.dir_owner {
        return AutoUpdate::OffOwner;
    }
    AutoUpdate::On
}

/// Why an install cannot be updated at all, by `update` or automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ineligible {
    /// The build is no release's.
    SourceBuild,
    /// No receipt is beside the binaries: the installer did not put them there.
    NoReceipt,
}

impl Ineligible {
    /// The refusal's `details.reason`.
    pub fn reason(self) -> &'static str {
        match self {
            Ineligible::SourceBuild => "not_a_release_build",
            Ineligible::NoReceipt => "no_receipt",
        }
    }
}

/// Whether `update` may replace an install: the build is a release's and a receipt is beside
/// the binaries, in that order. Nothing else stops a command the user ran: not the receipt's
/// `auto_update`, the environment, a container or a mark.
pub fn eligible(release_build: bool, has_receipt: bool) -> Result<(), Ineligible> {
    if !release_build {
        return Err(Ineligible::SourceBuild);
    }
    if !has_receipt {
        return Err(Ineligible::NoReceipt);
    }
    Ok(())
}

/// Whether a check for a newer release is due at `now`, when the last one was claimed at
/// `last` (the lock file's modification time; `None` when there is no lock file yet): after
/// [`CHECK_INTERVAL`]. A time in the future, as a clock set back leaves, counts as long ago.
pub fn check_due(now: SystemTime, last: Option<SystemTime>) -> bool {
    let Some(last) = last else {
        return true;
    };
    now.duration_since(last)
        .map_or(true, |since| since >= CHECK_INTERVAL)
}

/// What an automatic update left to say: [`NOTE_FILE`]'s one line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Note {
    /// `updated <from> <to>`: the install went from one version to a newer release.
    Updated {
        /// The version replaced.
        from: Version,
        /// The release installed.
        to: Version,
    },
    /// `available <to>`: a newer release is out and carries no mark for this build's line.
    Available {
        /// The release.
        to: Version,
    },
    /// `failed <to> <why>`: a newer release could not be installed.
    Failed {
        /// The release.
        to: Version,
        /// The installer's last line, on one line ([`Note::failed`]).
        why: String,
    },
}

impl Note {
    /// A note that `to` could not be installed, with the installer's last line: cut to one
    /// line of at most 200 characters, each control character a space, so that what a
    /// download printed cannot move a terminal's cursor.
    pub fn failed(to: Version, why: &str) -> Note {
        Note::Failed {
            to,
            why: one_line(why),
        }
    }

    /// The note `text` holds, the file's content. `None` when it is not a note.
    pub fn parse(text: &str) -> Option<Note> {
        let line = text.strip_suffix('\n').unwrap_or(text);
        if line.contains('\n') {
            return None;
        }
        let (word, rest) = line.split_once(' ')?;
        match word {
            UPDATED => {
                let (from, to) = rest.split_once(' ')?;
                Some(Note::Updated {
                    from: Version::parse(from)?,
                    to: Version::parse(to)?,
                })
            }
            AVAILABLE => Some(Note::Available {
                to: Version::parse(rest)?,
            }),
            FAILED => {
                let (to, why) = rest.split_once(' ').unwrap_or((rest, ""));
                Some(Note::failed(Version::parse(to)?, why))
            }
            _ => None,
        }
    }

    /// The file's content: the note's line.
    pub fn line(&self) -> String {
        match self {
            Note::Updated { from, to } => format!("{UPDATED} {from} {to}\n"),
            Note::Available { to } => format!("{AVAILABLE} {to}\n"),
            Note::Failed { to, why } => format!("{FAILED} {to} {}\n", one_line(why)),
        }
    }

    /// What a command at a terminal prints for the note, on stderr: a statement, and nothing
    /// that tells its reader to run a command.
    pub fn sentence(&self) -> String {
        match self {
            Note::Updated { from, to } => format!(
                "swarmops: updated from {from} to {to}. Notes: {}. Running mcp servers keep \
                 {from} until restarted.",
                notes_address(to)
            ),
            Note::Available { to } => format!(
                "swarmops: {to} is released and was not installed automatically. Notes: {}.",
                notes_address(to)
            ),
            Note::Failed { to, why } => {
                let why = one_line(why);
                match why.trim_end_matches('.') {
                    "" => format!("swarmops: {to} could not be installed automatically."),
                    why => format!("swarmops: {to} could not be installed automatically: {why}."),
                }
            }
        }
    }

    /// The note as `update --status` gives it to a script.
    pub fn pending(&self) -> UpdatePending {
        match self {
            Note::Updated { from, to } => {
                let mut pending = UpdatePending::new(UpdatePendingKind::Updated, to.to_string());
                pending.from = Some(from.to_string());
                pending
            }
            Note::Available { to } => {
                UpdatePending::new(UpdatePendingKind::Available, to.to_string())
            }
            Note::Failed { to, .. } => {
                UpdatePending::new(UpdatePendingKind::Failed, to.to_string())
            }
        }
    }
}

/// `text` on one line: its first [`MAX_WHY_CHARS`] characters, each control character a
/// space, without the spaces at its ends.
fn one_line(text: &str) -> String {
    let cut: String = text
        .chars()
        .take(MAX_WHY_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cut.trim().to_owned()
}

/// What automatic update remembers between checks: [`LOCK_FILE`]'s lines, one for each word.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// `seen <release>`: the latest release, found without a mark at the last check.
    seen: Option<Version>,
    /// `announced <release>`: the release an `available` note was written for.
    announced: Option<Version>,
    /// `failed <release>`: the release that could not be installed.
    failed: Option<Version>,
    /// The lines of other words, as read: a later build's.
    other: Vec<String>,
}

impl State {
    /// The state `text` holds, the lock file's content. A line of a known word that names no
    /// version says nothing; a line of any other word is kept as it is.
    pub fn parse(text: &str) -> State {
        let mut state = State::default();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
            let slot = match word {
                SEEN => &mut state.seen,
                ANNOUNCED => &mut state.announced,
                FAILED => &mut state.failed,
                _ => {
                    state.other.push(line.to_owned());
                    continue;
                }
            };
            if let Some(release) = Version::parse(rest) {
                *slot = Some(release);
            }
        }
        state
    }

    /// The file's content: a line for each word that names a release, then the lines kept.
    pub fn text(&self) -> String {
        let named = [
            (SEEN, &self.seen),
            (ANNOUNCED, &self.announced),
            (FAILED, &self.failed),
        ];
        let named = named
            .into_iter()
            .filter_map(|(word, release)| Some(format!("{word} {}\n", release.as_ref()?)));
        let other = self.other.iter().map(|line| format!("{line}\n"));
        named.chain(other).collect()
    }

    /// Records that `release`, the latest, carries no mark for this build's line, and gives
    /// the note to write if there is one: none at the first sighting, since a release is the
    /// latest before its marks are uploaded; `available` at the next check that finds it
    /// unmarked still; none ever after.
    pub fn sight_unmarked(&mut self, release: &Version) -> Option<Note> {
        if self.seen.as_ref() != Some(release) {
            self.seen = Some(release.clone());
            return None;
        }
        if self.announced.as_ref() == Some(release) {
            return None;
        }
        self.announced = Some(release.clone());
        Some(Note::Available {
            to: release.clone(),
        })
    }

    /// Whether `release` failed to install, so that it is not tried again automatically.
    pub fn has_failed(&self, release: &Version) -> bool {
        self.failed.as_ref() == Some(release)
    }

    /// Records that `release` could not be installed.
    pub fn record_failure(&mut self, release: &Version) {
        self.failed = Some(release.clone());
    }

    /// Forgets the failure, as an `update` the user ran does. Whether there was one.
    pub fn forget_failure(&mut self) -> bool {
        self.failed.take().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installer, whose receipt, file names and version rule this module shares.
    const INSTALLER: &str = include_str!("../../../scripts/install.sh");

    /// `text` as a version; it must be one.
    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text:?} is a version"))
    }

    #[test]
    fn a_version_is_three_numbers_and_a_pre_release() {
        for text in [
            "0.0.1",
            "0.12.0",
            "10.20.30",
            "1.2.3-rc.1",
            "1.2.3-a-b.7",
            "1.2.3--",
        ] {
            assert_eq!(v(text).to_string(), text);
        }
        // Leading zeros are the installer's to accept: the version is its numbers'.
        assert_eq!(v("01.002.3"), v("1.2.3"));

        for other in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "1.2.x",
            "1..3",
            ".1.2",
            "1.2.3-",
            "1.2.3+abc",
            "1.2.3-rc+abc",
            "1.2.3 ",
            " 1.2.3",
            "1.2.3\n",
            "1.2.3-rc 1",
            "1.2.3-\u{e9}",
            "+1.2.3",
            "1.-2.3",
            "99999999999999999999.0.0",
        ] {
            assert_eq!(Version::parse(other), None, "{other:?}");
        }
    }

    #[test]
    fn versions_order_by_their_numbers() {
        let ordered = ["0.9.9", "0.10.0", "0.10.1", "0.11.0", "1.0.0", "1.0.10"];
        for (i, lower) in ordered.iter().enumerate() {
            for higher in &ordered[i + 1..] {
                assert!(v(lower) < v(higher), "{lower} < {higher}");
                assert!(v(higher) > v(lower), "{higher} > {lower}");
                // Asked by the install's own version, as when both names are the same.
                assert!(
                    v(higher).replaces(Some(&v(lower)), &v(lower)),
                    "{higher} over {lower}"
                );
                assert!(
                    !v(lower).replaces(Some(&v(higher)), &v(higher)),
                    "{lower} over {higher}"
                );
            }
            assert_eq!(v(lower).partial_cmp(&v(lower)), Some(Ordering::Equal));
            assert!(
                !v(lower).replaces(Some(&v(lower)), &v(lower)),
                "{lower} over itself"
            );
        }
    }

    #[test]
    fn nothing_replaces_an_install_with_a_version_older_than_the_program_that_asks() {
        let (old, new, newer) = (v("0.12.0"), v("0.13.0"), v("0.14.0"));

        // 1. An install with no version, a name missing or another program's: the program
        //    that asks is the bound. Its own version writes the name again; an older one, as
        //    `latest` names after a release was deleted, replaces nothing.
        assert!(newer.replaces(None, &new));
        assert!(new.replaces(None, &new));
        assert!(!old.replaces(None, &new));

        // 2. An install stopped between the two renames: `swarmops` is new and `so` old, so
        //    the install is at the old version, whichever of the two asks.
        assert!(new.replaces(Some(&old), &old), "asked by so");
        assert!(new.replaces(Some(&old), &new), "asked by swarmops");
        // A release between the two would take `swarmops` back.
        assert!(!v("0.12.5").replaces(Some(&old), &new));

        // 3. Pre-releases: one is replaced by its release, and replaces neither its release
        //    nor another pre-release of it, with or without a version installed.
        let (rc1, rc2) = (v("0.13.0-rc.1"), v("0.13.0-rc.2"));
        assert!(new.replaces(None, &rc1));
        assert!(!rc1.replaces(None, &new));
        assert!(!rc2.replaces(None, &rc1));
        assert!(!rc2.replaces(Some(&old), &rc1));
        assert!(rc1.replaces(None, &rc1));
    }

    #[test]
    fn a_pre_release_orders_before_its_release_alone() {
        let (rc1, rc2, release) = (v("0.13.0-rc.1"), v("0.13.0-rc.2"), v("0.13.0"));

        // Before its release, after the release before, before the release after.
        assert!(rc1 < release);
        assert!(release > rc1);
        assert!(release.replaces(Some(&rc1), &rc1));
        assert!(!rc1.replaces(Some(&release), &release));
        assert!(rc1 > v("0.12.9") && rc1 < v("0.13.1"));
        assert!(v("0.13.1-rc.1") > release);

        // Two pre-releases of one release: in no order, so neither replaces the other.
        assert_eq!(rc1.partial_cmp(&rc2), None);
        assert!(!rc1.replaces(Some(&rc2), &rc2) && !rc2.replaces(Some(&rc1), &rc1));
        assert_eq!(rc1.partial_cmp(&rc1.clone()), Some(Ordering::Equal));

        assert!(release.is_release() && !rc1.is_release());
        assert_eq!(rc1.minor_line(), "0.13");
        assert_eq!(v("12.3.45").minor_line(), "12.3");
    }

    #[test]
    fn a_version_line_is_the_program_s_name_and_a_version() {
        let named = [
            ("swarmops 0.11.0", "0.11.0"),
            ("swarmops 0.11.0\n", "0.11.0"),
            ("swarmops 1.0.0-rc.1\n", "1.0.0-rc.1"),
            ("swarmops 0.11.0+69e435e\n", "0.11.0"),
            ("swarmops 0.11.0-rc.1+69e435e36a67", "0.11.0-rc.1"),
        ];
        for (line, version) in named {
            assert_eq!(Version::of_line(line), Some(v(version)), "{line:?}");
        }
        for other in [
            "",
            "swarmops",
            "swarmops ",
            "so 0.11.0",
            "0.11.0",
            "Swarmops 0.11.0",
            "swarmops  0.11.0",
            "swarmops 0.11.0 extra",
            "swarmops 0.11.0+",
            "swarmops 0.11.0+abc def",
            "swarmops 0.11.0\nargv: --version\n",
            "swarmops 0.11.0\n\n",
            "\nswarmops 0.11.0",
            "swarmops 0.11.0\r\n",
        ] {
            assert_eq!(Version::of_line(other), None, "{other:?}");
        }
    }

    #[test]
    fn an_install_is_at_the_lower_of_its_two_names() {
        let at = |swarmops: Option<&str>, so: Option<&str>| {
            installed(swarmops.map(v), so.map(v)).map(|version| version.to_string())
        };
        assert_eq!(
            at(Some("0.12.0"), Some("0.12.0")).as_deref(),
            Some("0.12.0")
        );
        // Stopped between the two renames: `swarmops` is new and `so` old.
        assert_eq!(
            at(Some("0.13.0"), Some("0.12.0")).as_deref(),
            Some("0.12.0")
        );
        assert_eq!(
            at(Some("0.12.0"), Some("0.13.0")).as_deref(),
            Some("0.12.0")
        );
        assert_eq!(
            at(Some("0.13.0"), Some("0.13.0-rc.1")).as_deref(),
            Some("0.13.0-rc.1")
        );
        // A name that is missing or not SwarmOps' leaves the install without a version.
        assert_eq!(at(None, Some("0.12.0")), None);
        assert_eq!(at(Some("0.12.0"), None), None);
        assert_eq!(at(None, None), None);
        // In no order: `swarmops`'s, and what replaces one replaces the other.
        assert_eq!(
            at(Some("0.13.0-rc.1"), Some("0.13.0-rc.2")).as_deref(),
            Some("0.13.0-rc.1")
        );
    }

    #[test]
    fn the_addresses_are_the_releases_page_s() {
        assert_eq!(
            latest_address(),
            "https://github.com/KeiaiLab/SwarmOps/releases/latest"
        );
        assert_eq!(
            notes_address(&v("0.13.0")),
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0"
        );
        // The mark names the release it is in and the line of the build that asks.
        assert_eq!(
            mark_address(&v("0.13.0"), &v("0.12.4")),
            "https://github.com/KeiaiLab/SwarmOps/releases/download/v0.13.0/auto-update-from-0.12"
        );
        assert_eq!(
            mark_address(&v("1.2.0"), &v("1.0.0-rc.1")),
            "https://github.com/KeiaiLab/SwarmOps/releases/download/v1.2.0/auto-update-from-1.0"
        );
        // The installer finds its releases at the same place.
        let repo = RELEASES_URL.strip_suffix("/releases").expect("a suffix");
        assert!(
            INSTALLER.contains(&format!("REPO_URL='{repo}'\n")),
            "{repo}"
        );
        assert!(INSTALLER.contains("RELEASES_URL=\"$REPO_URL/releases\"\n"));
    }

    #[test]
    fn a_redirect_names_a_release_only_as_its_exact_page() {
        assert_eq!(
            release_at("https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0"),
            Some(v("0.13.0"))
        );
        assert_eq!(release_at(&notes_address(&v("10.0.2"))), Some(v("10.0.2")));

        for other in [
            // What curl prints for an answer that redirects nowhere, such as a portal's 200.
            "",
            "https://github.com/KeiaiLab/SwarmOps/releases",
            "https://github.com/KeiaiLab/SwarmOps/releases/latest",
            // Another scheme, host, repository or page.
            "http://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0",
            "https://github.com.example.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0",
            "https://example.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0",
            "https://github.com/Other/SwarmOps/releases/tag/v0.13.0",
            "https://github.com/KeiaiLab/SwarmOps/releases/download/v0.13.0",
            "https://github.com/login?return_to=/KeiaiLab/SwarmOps/releases/tag/v0.13.0",
            // A tag that is no release's three plain numbers, or more than the tag.
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/0.13.0",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0-rc.1",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v00.13.0",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0/",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0?x=1",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0#top",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0\n",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0\nx",
            " https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0 ",
            "https://github.com/KeiaiLab/SwarmOps/releases/tag/v../../x",
            "<html>",
        ] {
            assert_eq!(release_at(other), None, "{other:?}");
        }
    }

    #[test]
    fn the_installer_s_receipt_reads_and_writes_back_byte_for_byte() {
        for (on, text) in [
            (true, "receipt = 1\nauto_update = true\n"),
            (false, "receipt = 1\nauto_update = false\n"),
        ] {
            // The installer writes exactly this text, by a printf of its own.
            let printed = text.replace('\n', "\\n");
            assert!(INSTALLER.contains(&format!("printf '{printed}'")), "{text}");

            let receipt = Receipt::parse(text).expect("the installer's receipt");
            assert_eq!(receipt.auto_update(), on);
            assert_eq!(receipt.text().as_deref(), Ok(text));
        }

        // Turned off and on again, as `update --auto` does.
        let mut receipt = Receipt::parse("receipt = 1\nauto_update = true\n").expect("a receipt");
        receipt.set_auto_update(false);
        assert_eq!(
            receipt.text().as_deref(),
            Ok("receipt = 1\nauto_update = false\n")
        );
        receipt.set_auto_update(true);
        assert_eq!(
            receipt.text().as_deref(),
            Ok("receipt = 1\nauto_update = true\n")
        );

        // Another layout of the same two keys.
        let spaced = "# by hand\nauto_update=false # no\n\nreceipt   =   1\n";
        assert!(!Receipt::parse(spaced).expect("a receipt").auto_update());
    }

    #[test]
    fn a_rewritten_receipt_keeps_the_keys_this_build_does_not_know() {
        let text = concat!(
            "receipt = 1\n",
            "auto_update = true\n",
            "channel = \"stable\"\n",
            "installed_by = \"install.sh\"\n",
            "\n",
            "[origin]\n",
            "url = \"https://example.com\"\n",
            "tries = 3\n",
            "\n",
            "[[history]]\n",
            "version = \"0.12.0\"\n",
            "\n",
            "[[history]]\n",
            "version = \"0.13.0\"\n",
        );
        let mut receipt = Receipt::parse(text).expect("a receipt with more keys");
        receipt.set_auto_update(false);
        let written = receipt.text().expect("text");

        // The installer's `receipt_says_off` reads this line.
        assert!(
            written.starts_with("receipt = 1\nauto_update = false\n"),
            "{written}"
        );
        let read = Receipt::parse(&written).expect("what was written reads");
        assert_eq!(read, receipt);
        let table = written.parse::<toml::Table>().expect("TOML");
        assert_eq!(table["channel"].as_str(), Some("stable"));
        assert_eq!(table["installed_by"].as_str(), Some("install.sh"));
        assert_eq!(table["origin"]["url"].as_str(), Some("https://example.com"));
        assert_eq!(table["origin"]["tries"].as_integer(), Some(3));
        let history = table["history"].as_array().expect("an array of tables");
        assert_eq!(history.len(), 2);
        assert_eq!(history[1]["version"].as_str(), Some("0.13.0"));
        assert_eq!(table.len(), 6, "{written}");
    }

    #[test]
    fn a_file_that_is_no_receipt_is_refused_with_its_reason() {
        let cases = [
            ("", ReceiptFault::Version),
            ("auto_update = true\n", ReceiptFault::Version),
            ("receipt = 2\nauto_update = true\n", ReceiptFault::Version),
            ("receipt = 0\nauto_update = true\n", ReceiptFault::Version),
            (
                "receipt = \"1\"\nauto_update = true\n",
                ReceiptFault::Version,
            ),
            ("receipt = 1.0\nauto_update = true\n", ReceiptFault::Version),
            (
                "receipt = true\nauto_update = true\n",
                ReceiptFault::Version,
            ),
            ("version = 1\nauto_update = true\n", ReceiptFault::Version),
            ("receipt = 1\n", ReceiptFault::AutoUpdate),
            ("receipt = 1\nauto_update = 1\n", ReceiptFault::AutoUpdate),
            (
                "receipt = 1\nauto_update = \"true\"\n",
                ReceiptFault::AutoUpdate,
            ),
            ("receipt = 1\nauto_update = on\n", ReceiptFault::NotToml),
            ("receipt = 1\nauto_update = True\n", ReceiptFault::NotToml),
            (
                "receipt = 1\nreceipt = 1\nauto_update = true\n",
                ReceiptFault::NotToml,
            ),
            ("receipt 1\n", ReceiptFault::NotToml),
            (
                "{\"receipt\": 1, \"auto_update\": true}",
                ReceiptFault::NotToml,
            ),
            ("[receipt]\nauto_update = true\n", ReceiptFault::Version),
        ];
        for (text, fault) in cases {
            assert_eq!(Receipt::parse(text), Err(fault), "{text:?}");
        }
        for fault in [
            ReceiptFault::NotToml,
            ReceiptFault::Version,
            ReceiptFault::AutoUpdate,
            ReceiptFault::Unwritable,
        ] {
            assert!(!fault.why().is_empty(), "{fault:?}");
        }
    }

    #[test]
    fn the_files_have_the_names_the_installer_removes() {
        assert!(
            INSTALLER.contains(&format!("\nRECEIPT={RECEIPT_FILE}\n")),
            "the receipt"
        );
        let left = format!("\nUPDATE_FILES='{LOCK_FILE} {NOTE_FILE} {STAGING_DIR}'\n");
        assert!(INSTALLER.contains(&left), "{left}");
    }

    #[test]
    fn the_switch_is_on_off_or_no_switch() {
        let of = |value: &str| Switch::of(Some(OsStr::new(value)));
        assert_eq!(Switch::of(None), Switch::Unset);
        assert_eq!(of(""), Switch::Unset);
        assert_eq!(of("on"), Switch::On);
        assert_eq!(of("off"), Switch::Off);
        for other in [
            "OFF", "Off", "ON", "0", "1", "false", "true", "no", "off ", " off", "on\n", "never",
        ] {
            assert_eq!(of(other), Switch::Invalid, "{other:?}");
        }
        {
            use std::os::unix::ffi::OsStrExt as _;
            assert_eq!(
                Switch::of(Some(OsStr::from_bytes(b"of\xff"))),
                Switch::Invalid
            );
        }

        let env = |name: &str| (name == AUTO_UPDATE_ENV).then(|| OsString::from("off"));
        assert_eq!(Switch::read(&env), Switch::Off);
        assert_eq!(Switch::read(&|_| None), Switch::Unset);
        assert_eq!(AUTO_UPDATE_ENV, "SWARMOPS_AUTO_UPDATE");
    }

    #[test]
    fn ci_is_any_value_but_empty_0_and_false() {
        let of = |value: &str| ci_of(Some(OsStr::new(value)));
        assert!(!ci_of(None));
        for unset in ["", "0", "false"] {
            assert!(!of(unset), "{unset:?}");
        }
        for set in [
            "true",
            "1",
            "yes",
            "False",
            "FALSE",
            "00",
            " ",
            "woodpecker",
        ] {
            assert!(of(set), "{set:?}");
        }
        let env = |name: &str| (name == "CI").then(|| OsString::from("true"));
        assert!(in_ci(&env));
        assert!(!in_ci(&|_| None));
    }

    /// Facts under which automatic update is on: a user's own install, by the installer.
    fn on() -> Facts {
        Facts::new(true, 501, 501)
    }

    #[test]
    fn automatic_update_is_on_for_a_user_s_own_install() {
        assert_eq!(auto_update(&on()), AutoUpdate::On);
        // `on` in the variable changes nothing there.
        let mut forced = on();
        forced.switch = Switch::On;
        assert_eq!(auto_update(&forced), AutoUpdate::On);
    }

    /// Each rule alone turns it off with its own word, and each rule wins over every later
    /// one, whatever those say.
    #[test]
    fn the_first_rule_that_holds_decides() {
        // The rules in order, each as a change to the facts of `on()`.
        type Rule = (AutoUpdate, fn(&mut Facts));
        let rules: [Rule; 7] = [
            (AutoUpdate::OffSourceBuild, |f| f.release_build = false),
            (AutoUpdate::OffNoReceipt, |f| f.receipt = None),
            (AutoUpdate::Off, |f| f.receipt = Some(false)),
            (AutoUpdate::OffEnv, |f| f.switch = Switch::Off),
            (AutoUpdate::OffCi, |f| f.ci = true),
            (AutoUpdate::OffContainer, |f| f.container = true),
            (AutoUpdate::OffOwner, |f| f.euid = 0),
        ];
        for (i, (answer, holds)) in rules.iter().enumerate() {
            // 1. Alone.
            let mut alone = on();
            holds(&mut alone);
            assert_eq!(auto_update(&alone), *answer, "{answer:?} alone");

            // 2. With every later rule holding too. (Rules 2 and 3 are both the receipt's, so
            //    a later rule's change is applied first and this rule's last.)
            let mut with_later = on();
            for (_, later) in &rules[i + 1..] {
                later(&mut with_later);
            }
            holds(&mut with_later);
            assert_eq!(auto_update(&with_later), *answer, "{answer:?} first");

            // 3. With one later rule at a time.
            for (other, later) in &rules[i + 1..] {
                let mut pair = on();
                later(&mut pair);
                holds(&mut pair);
                assert_eq!(auto_update(&pair), *answer, "{answer:?} before {other:?}");
            }
        }
    }

    #[test]
    fn a_value_that_is_no_switch_counts_as_off() {
        let mut facts = on();
        facts.switch = Switch::Invalid;
        assert_eq!(auto_update(&facts), AutoUpdate::OffEnv);
        // Also where `on` would have kept it on.
        facts.ci = true;
        facts.container = true;
        assert_eq!(auto_update(&facts), AutoUpdate::OffEnv);
    }

    #[test]
    fn on_in_the_variable_cancels_ci_and_the_container_and_nothing_else() {
        let forced = || {
            let mut facts = on();
            facts.switch = Switch::On;
            facts
        };

        let mut ci = forced();
        ci.ci = true;
        assert_eq!(auto_update(&ci), AutoUpdate::On);
        let mut container = forced();
        container.container = true;
        assert_eq!(auto_update(&container), AutoUpdate::On);
        let mut both = forced();
        both.ci = true;
        both.container = true;
        assert_eq!(auto_update(&both), AutoUpdate::On);

        // The build, the receipt and the owner stand.
        let mut source = both;
        source.release_build = false;
        assert_eq!(auto_update(&source), AutoUpdate::OffSourceBuild);
        let mut packaged = both;
        packaged.receipt = None;
        assert_eq!(auto_update(&packaged), AutoUpdate::OffNoReceipt);
        let mut said_off = both;
        said_off.receipt = Some(false);
        assert_eq!(auto_update(&said_off), AutoUpdate::Off);
        let mut root = both;
        root.euid = 0;
        assert_eq!(auto_update(&root), AutoUpdate::OffOwner);
    }

    #[test]
    fn root_and_another_user_s_directory_are_off() {
        let owned = |euid, dir_owner| auto_update(&Facts::new(true, euid, dir_owner));
        assert_eq!(owned(501, 501), AutoUpdate::On);
        // Root, also in root's own directory.
        assert_eq!(owned(0, 0), AutoUpdate::OffOwner);
        assert_eq!(owned(0, 501), AutoUpdate::OffOwner);
        // A directory of root's or of another user's, writable or not.
        assert_eq!(owned(501, 0), AutoUpdate::OffOwner);
        assert_eq!(owned(501, 502), AutoUpdate::OffOwner);
    }

    #[test]
    fn an_update_the_user_runs_needs_a_release_s_build_and_a_receipt() {
        assert_eq!(eligible(true, true), Ok(()));
        assert_eq!(eligible(true, false), Err(Ineligible::NoReceipt));
        assert_eq!(eligible(false, true), Err(Ineligible::SourceBuild));
        assert_eq!(eligible(false, false), Err(Ineligible::SourceBuild));
        assert_eq!(Ineligible::SourceBuild.reason(), "not_a_release_build");
        assert_eq!(Ineligible::NoReceipt.reason(), "no_receipt");
    }

    #[test]
    fn a_check_is_due_a_day_after_the_last() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(500_000);
        let ago = |seconds: u64| Some(now - Duration::from_secs(seconds));
        let day = 24 * 60 * 60;
        assert_eq!(CHECK_INTERVAL, Duration::from_secs(day));

        // No lock file yet: never checked.
        assert!(check_due(now, None));
        // Within the day.
        assert!(!check_due(now, Some(now)));
        assert!(!check_due(now, ago(1)));
        assert!(!check_due(now, ago(day - 1)));
        // From a day on.
        assert!(check_due(now, ago(day)));
        assert!(check_due(now, ago(day + 1)));
        assert!(check_due(now, ago(400 * day)));
        // A time in the future is stale, however near or far.
        assert!(check_due(now, Some(now + Duration::from_secs(1))));
        assert!(check_due(now, Some(now + Duration::from_secs(400 * day))));
    }

    #[test]
    fn a_note_is_one_line_of_three_kinds() {
        let notes = [
            (
                Note::Updated {
                    from: v("0.12.0"),
                    to: v("0.13.0"),
                },
                "updated 0.12.0 0.13.0\n",
            ),
            (Note::Available { to: v("0.13.0") }, "available 0.13.0\n"),
            (
                Note::failed(v("0.13.0"), "SwarmOps 0.13.0 needs glibc 2.36 or newer"),
                "failed 0.13.0 SwarmOps 0.13.0 needs glibc 2.36 or newer\n",
            ),
            (Note::failed(v("0.13.0"), ""), "failed 0.13.0 \n"),
        ];
        for (note, line) in notes {
            assert_eq!(note.line(), line);
            assert_eq!(Note::parse(line), Some(note.clone()), "{line:?}");
            let bare = line.strip_suffix('\n').expect("a line");
            assert_eq!(Note::parse(bare), Some(note), "{bare:?}");
        }
        assert_eq!(
            Note::parse("failed 0.13.0"),
            Some(Note::failed(v("0.13.0"), ""))
        );

        for other in [
            "",
            "\n",
            "updated",
            "updated 0.12.0",
            "updated 0.12.0 0.13.0 more",
            "updated v0.12.0 0.13.0",
            "available",
            "available 0.13",
            "available 0.13.0 more",
            "failed",
            "failed why",
            "Updated 0.12.0 0.13.0",
            "removed 0.13.0",
            " available 0.13.0",
            "available 0.13.0\navailable 0.14.0\n",
            "available 0.13.0\n\n",
        ] {
            assert_eq!(Note::parse(other), None, "{other:?}");
        }
    }

    #[test]
    fn a_note_s_sentence_states_and_does_not_instruct() {
        let updated = Note::Updated {
            from: v("0.12.0"),
            to: v("0.13.0"),
        };
        assert_eq!(
            updated.sentence(),
            "swarmops: updated from 0.12.0 to 0.13.0. Notes: \
             https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0. Running mcp servers \
             keep 0.12.0 until restarted."
        );
        let available = Note::Available { to: v("0.13.0") };
        assert_eq!(
            available.sentence(),
            "swarmops: 0.13.0 is released and was not installed automatically. Notes: \
             https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0."
        );
        let failed = Note::failed(v("0.13.0"), "SwarmOps 0.13.0 has no macOS build.");
        assert_eq!(
            failed.sentence(),
            "swarmops: 0.13.0 could not be installed automatically: SwarmOps 0.13.0 has no \
             macOS build."
        );
        assert_eq!(
            Note::failed(v("0.13.0"), " \n").sentence(),
            "swarmops: 0.13.0 could not be installed automatically."
        );

        // No sentence names a command to run: an agent that reads stderr is told nothing to do.
        for note in [updated, available, failed] {
            let sentence = note.sentence();
            assert!(sentence.starts_with("swarmops: "), "{sentence}");
            assert!(!sentence.contains('\n'), "{sentence}");
            for command in ["so update", "swarmops update", "run:", "--auto"] {
                assert!(!sentence.contains(command), "{sentence}");
            }
        }
    }

    #[test]
    fn a_failure_s_reason_stays_on_one_line() {
        // What a download printed cannot add a line to the file or move a terminal's cursor.
        let hostile = "first\nupdated 0.1.0 9.9.9\r\x1b[2Jlast\t.";
        let note = Note::failed(v("0.13.0"), hostile);
        let Note::Failed { why, .. } = &note else {
            panic!("a failure's note");
        };
        assert_eq!(why, "first updated 0.1.0 9.9.9  [2Jlast .");
        assert_eq!(note.line().matches('\n').count(), 1);
        assert_eq!(Note::parse(&note.line()), Some(note.clone()));
        assert!(!note.sentence().contains(['\n', '\r', '\x1b', '\t']));

        // A variant built by hand is cut the same when written or said.
        let by_hand = Note::Failed {
            to: v("0.13.0"),
            why: "a\nb".to_owned(),
        };
        assert_eq!(by_hand.line(), "failed 0.13.0 a b\n");
        assert!(by_hand.sentence().ends_with(": a b."));

        // At most 200 characters, counted as characters.
        let long = "\u{e9}".repeat(500);
        let Note::Failed { why, .. } = Note::failed(v("0.13.0"), &long) else {
            panic!("a failure's note");
        };
        assert_eq!(why.chars().count(), 200);
    }

    #[test]
    fn a_note_is_given_to_a_script_as_its_kind_and_versions() {
        let updated = Note::Updated {
            from: v("0.12.0"),
            to: v("0.13.0"),
        }
        .pending();
        assert_eq!(
            (updated.kind, updated.from.as_deref(), updated.to.as_str()),
            (UpdatePendingKind::Updated, Some("0.12.0"), "0.13.0")
        );
        let available = Note::Available { to: v("0.13.0") }.pending();
        assert_eq!(
            (available.kind, available.from, available.to.as_str()),
            (UpdatePendingKind::Available, None, "0.13.0")
        );
        let failed = Note::failed(v("0.13.0"), "why").pending();
        assert_eq!(
            (failed.kind, failed.from, failed.to.as_str()),
            (UpdatePendingKind::Failed, None, "0.13.0")
        );
    }

    #[test]
    fn the_state_is_a_line_for_each_word() {
        assert_eq!(State::parse(""), State::default());
        assert_eq!(State::default().text(), "");

        let text = "seen 0.13.0\nannounced 0.12.0\nfailed 0.11.1\n";
        let state = State::parse(text);
        assert_eq!(state.seen, Some(v("0.13.0")));
        assert_eq!(state.announced, Some(v("0.12.0")));
        assert_eq!(state.failed, Some(v("0.11.1")));
        assert_eq!(state.text(), text);

        // In any order, with blank lines; a word met twice says what its last line says.
        let mixed = State::parse("failed 0.11.1\n\nseen 0.12.0\nseen 0.13.0\nannounced 0.12.0");
        assert_eq!(mixed, state);
    }

    #[test]
    fn the_state_keeps_the_lines_it_does_not_know_and_drops_broken_ones() {
        let text = concat!(
            "seen 0.13.0\n",
            "checked 1800000000\n",
            "failed not-a-version\n",
            "announced\n",
            "Seen 0.14.0\n",
            "held\n",
        );
        let mut state = State::parse(text);
        assert_eq!(state.seen, Some(v("0.13.0")));
        assert_eq!((&state.announced, &state.failed), (&None, &None));
        assert_eq!(state.other, ["checked 1800000000", "Seen 0.14.0", "held"]);

        state.record_failure(&v("0.13.0"));
        assert_eq!(
            state.text(),
            "seen 0.13.0\nfailed 0.13.0\nchecked 1800000000\nSeen 0.14.0\nheld\n"
        );
        assert_eq!(State::parse(&state.text()), state);
    }

    #[test]
    fn an_unmarked_release_is_announced_at_its_second_sighting_once() {
        let mut state = State::default();
        let release = v("0.13.0");

        // 1. First sighting: the marks may not be uploaded yet. Nothing is said.
        assert_eq!(state.sight_unmarked(&release), None);
        assert_eq!(state.text(), "seen 0.13.0\n");

        // 2. The next check, a day later, as a new process reads it: still unmarked.
        let mut state = State::parse(&state.text());
        assert_eq!(
            state.sight_unmarked(&release),
            Some(Note::Available {
                to: release.clone()
            })
        );
        assert_eq!(state.text(), "seen 0.13.0\nannounced 0.13.0\n");

        // 3. Every check after that: nothing more.
        for _ in 0..3 {
            let mut later = State::parse(&state.text());
            assert_eq!(later.sight_unmarked(&release), None);
            assert_eq!(later, state);
        }

        // 4. A newer release starts over: seen, then announced.
        let newer = v("0.14.0");
        assert_eq!(state.sight_unmarked(&newer), None);
        assert_eq!(state.text(), "seen 0.14.0\nannounced 0.13.0\n");
        assert_eq!(
            state.sight_unmarked(&newer),
            Some(Note::Available { to: newer.clone() })
        );
        assert_eq!(state.sight_unmarked(&newer), None);
        assert_eq!(state.text(), "seen 0.14.0\nannounced 0.14.0\n");
    }

    #[test]
    fn a_release_that_failed_is_remembered_until_the_user_updates() {
        let mut state = State::default();
        let release = v("0.13.0");
        assert!(!state.has_failed(&release));
        assert!(!state.forget_failure());

        state.record_failure(&release);
        assert_eq!(state.text(), "failed 0.13.0\n");
        let mut read = State::parse(&state.text());
        assert!(read.has_failed(&release));
        // The release after it is tried.
        assert!(!read.has_failed(&v("0.13.1")));
        assert!(!read.has_failed(&v("0.13.0-rc.1")));

        // An `update` the user runs clears it, and says whether there was one.
        assert!(read.forget_failure());
        assert!(!read.has_failed(&release));
        assert!(!read.forget_failure());
        assert_eq!(read.text(), "");
    }
}
