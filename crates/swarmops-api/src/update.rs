//! The program's own update: the result of `update`, and whether an install updates itself.
//!
//! This is a client-side operation on the program's files, beside no store: no `StoreOps` has
//! it and no MCP tool runs it. Like the logins' types, [`UpdateOut`] carries a path of the
//! caller's host: `dir`, the directory the binaries are installed in.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Whether an install updates itself after a command, and when it does not, what decided so.
/// The values after `on` are in the order that decides: the first that holds is the answer.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AutoUpdate {
    /// Automatic update is on: nothing below holds.
    On,
    /// Off: the build is no release's, but one from a clone of the source.
    OffSourceBuild,
    /// Off: no receipt of the installer's is beside the binaries, as for a package's install
    /// or an archive unpacked by hand.
    OffNoReceipt,
    /// Off: the receipt says so, as `update --auto off` or the installer wrote it.
    Off,
    /// Off: `SWARMOPS_AUTO_UPDATE` is `off`, or holds a value that is neither `on` nor `off`.
    OffEnv,
    /// Off: `CI` is set, and `SWARMOPS_AUTO_UPDATE` is not `on`.
    OffCi,
    /// Off: the host is a container, and `SWARMOPS_AUTO_UPDATE` is not `on`.
    OffContainer,
    /// Off: the caller is root, or the install directory is another user's.
    OffOwner,
}

/// The kind of a note an automatic update left: what became of a newer release.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UpdatePendingKind {
    /// A newer release was installed.
    Updated,
    /// A newer release is out and was not installed automatically.
    Available,
    /// A newer release could not be installed, and is not tried again automatically: it was
    /// downloaded and refused, or could not be downloaded at three checks.
    Failed,
}

/// A note an automatic update left: the next command at a terminal prints it, once, and it is
/// kept until then.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpdatePending {
    /// What happened.
    pub kind: UpdatePendingKind,
    /// The version that was replaced. There for `updated` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The release the note is about: the one installed, out, or not installed.
    pub to: String,
}

impl UpdatePending {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(kind: UpdatePendingKind, to: impl Into<String>) -> Self {
        Self {
            kind,
            from: None,
            to: to.into(),
        }
    }
}

/// `update`: what the program's version is now, and whether the install updates itself.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpdateOut {
    /// The installed version when the command ended. For the forms that ask no binary for
    /// its version (`--status`, `--auto`, a `--background` that stops before it looks), and
    /// where one of the two names has none, the running program's own.
    pub current: String,
    /// The version this run replaced. Omitted when nothing was replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// The latest release. Omitted when the command did not look for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// Whether this run replaced the program. False when the latest release is not newer than
    /// what is installed, and for the forms of the command that install nothing.
    pub updated: bool,
    /// Whether the install updates itself after a command, and if not, why.
    pub auto_update: AutoUpdate,
    /// The directory the binaries are installed in, a path on the caller's host. Omitted when
    /// no receipt is beside them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// The page of the latest release's notes. Omitted when the command did not look for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// The note an automatic update left that no command has printed yet. Omitted when there
    /// is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<UpdatePending>,
}

impl UpdateOut {
    /// Takes the required fields; the others get their serde defaults.
    pub fn new(current: impl Into<String>, updated: bool, auto_update: AutoUpdate) -> Self {
        Self {
            current: current.into(),
            previous: None,
            latest: None,
            updated,
            auto_update,
            dir: None,
            notes: None,
            pending: None,
        }
    }
}
