//! The transport of the git remotes (M13): the user's own git, spawned against the store's
//! mirror, moves the packs; SwarmOps decides what moves (operations spec §6).
//!
//! ```text
//!   swarmops-ops (fetch, push, sync)
//!          │  Transport (this module's trait)
//!          ▼
//!   GitCli ── spawns ──▶ git fetch / git push / git ls-remote ──▶ the remote
//!          │                 GIT_DIR = <store>/git/mirror.git
//!          ▼
//!   the classifier: exit status, push's porcelain, git's messages ──▶ TransportError
//! ```
//!
//! One function builds every child, so every rule holds for every call: the binary
//! (`SWARMOPS_GIT`, else `git` on `PATH`, 2.39 or later), an environment cleaned of the variables
//! that point git at another repository, configuration passed in the environment and never in the
//! arguments (`core.fsync=all` among it), `--end-of-options` before every URL and refspec, no
//! shell, and a push that never forces. A token a remote names reaches git as a header scoped to
//! the remote's URL, and is redacted from everything git prints before it enters an error.
//!
//! The M13 plan's task C1 froze these items; task C3 writes their bodies, which until then
//! panic with the task that writes them.

use std::path::Path;

use crate::oid::GitOid;

/// The task that writes this module's bodies.
const PLANNED: &str = "planned in M13 (C3)";

/// What moves bytes between the mirror and a remote. [`GitCli`] is the one implementation;
/// a native transport would be a second, and its callers would not change.
pub trait Transport {
    /// The branch the remote's `HEAD` names, by its short name (`git ls-remote --symref <url>
    /// HEAD`); `None` when the remote names none (an empty repository, or a server that does not
    /// advertise an unborn `HEAD`).
    fn ls_remote_head(
        &self,
        url: &RemoteUrl,
        cred: &Credential,
    ) -> Result<Option<String>, TransportError>;

    /// Fetches `refspecs` from the remote at `url` into the bare repository `mirror`.
    fn fetch(
        &self,
        mirror: &Path,
        url: &RemoteUrl,
        cred: &Credential,
        refspecs: &[RefSpec],
    ) -> Result<(), TransportError>;

    /// Sends `update` from `mirror` to the remote at `url`, by compare-and-swap: the remote
    /// branch moves only from `update.expect` (absent when `None`), and never by force.
    fn push(
        &self,
        mirror: &Path,
        url: &RemoteUrl,
        cred: &Credential,
        update: &PushUpdate,
    ) -> Result<PushOutcome, TransportError>;

    /// The objects `new` reaches that `old` does not, in `mirror`: each blob with its size and
    /// path, and their bytes on disk together (`git rev-list --objects --disk-usage`).
    fn new_objects(
        &self,
        mirror: &Path,
        new: GitOid,
        old: Option<GitOid>,
    ) -> Result<NewObjects, TransportError>;

    /// Runs git's own automatic housekeeping on `mirror`, in the foreground (`git gc --auto`).
    fn gc_auto(&self, mirror: &Path) -> Result<(), TransportError>;

    /// Whether `ancestor` is `descendant` or one of its ancestors, in `mirror`.
    fn is_ancestor(
        &self,
        mirror: &Path,
        ancestor: GitOid,
        descendant: GitOid,
    ) -> Result<bool, TransportError>;
}

/// The user's git, as a [`Transport`].
#[derive(Debug, Clone)]
pub struct GitCli {
    /// The binary: `SWARMOPS_GIT`, else `git` as `PATH` finds it.
    binary: std::path::PathBuf,
}

impl GitCli {
    /// Locates the binary; fails only with [`TransportError::GitMissing`]. Its version is read
    /// by the first command it runs, so a push that sends nothing runs no git.
    pub fn find() -> Result<GitCli, TransportError> {
        unimplemented!("{PLANNED}")
    }

    /// The binary this runs.
    pub fn binary(&self) -> &Path {
        &self.binary
    }
}

impl Transport for GitCli {
    fn ls_remote_head(
        &self,
        _url: &RemoteUrl,
        _cred: &Credential,
    ) -> Result<Option<String>, TransportError> {
        unimplemented!("{PLANNED}")
    }

    fn fetch(
        &self,
        _mirror: &Path,
        _url: &RemoteUrl,
        _cred: &Credential,
        _refspecs: &[RefSpec],
    ) -> Result<(), TransportError> {
        unimplemented!("{PLANNED}")
    }

    fn push(
        &self,
        _mirror: &Path,
        _url: &RemoteUrl,
        _cred: &Credential,
        _update: &PushUpdate,
    ) -> Result<PushOutcome, TransportError> {
        unimplemented!("{PLANNED}")
    }

    fn new_objects(
        &self,
        _mirror: &Path,
        _new: GitOid,
        _old: Option<GitOid>,
    ) -> Result<NewObjects, TransportError> {
        unimplemented!("{PLANNED}")
    }

    fn gc_auto(&self, _mirror: &Path) -> Result<(), TransportError> {
        unimplemented!("{PLANNED}")
    }

    fn is_ancestor(
        &self,
        _mirror: &Path,
        _ancestor: GitOid,
        _descendant: GitOid,
    ) -> Result<bool, TransportError> {
        unimplemented!("{PLANNED}")
    }
}

/// A remote's URL that the rules accept: `https://`, `ssh://` or the scp form, and plain
/// `http://` to a loopback address only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUrl {
    /// The URL, as given.
    url: String,
}

impl RemoteUrl {
    /// `url`, checked against the rules.
    pub fn parse(_url: &str) -> Result<RemoteUrl, UrlRefused> {
        unimplemented!("{PLANNED}")
    }

    /// The host the URL names, without user or port.
    pub fn host(&self) -> &str {
        unimplemented!("{PLANNED}")
    }

    /// The URL, as given.
    pub fn as_str(&self) -> &str {
        &self.url
    }
}

/// Why a URL is refused (`E_INVALID_ARG`, `details.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRefused {
    /// `http://` to a host that is not a loopback address: `cleartext_http`.
    CleartextHttp,
    /// `file://`, a local path, `ext::`, `fd::` or any other scheme: `url_scheme`.
    Scheme,
    /// A URL with a password: `credential_in_url`.
    CredentialInUrl,
    /// A URL or a host that starts with `-`: `url_option`.
    OptionLike,
}

/// How git authenticates to a remote: by git's own credentials, and by the token in the
/// variable `token_env` names when it is set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credential {
    /// The environment variable that holds a token, by its name; never the token.
    pub token_env: Option<String>,
}

/// A fetch refspec: the remote's refs that `src` matches, stored in the mirror as `dst`. The
/// mirror's refs record what the remote holds, so a fetch moves them to it whatever it was
/// (`+src:dst`); a push never takes a refspec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSpec {
    /// The remote's refs, such as `refs/heads/*`.
    pub src: String,
    /// Where the mirror keeps them, such as `refs/remotes/origin/*`.
    pub dst: String,
}

/// One branch update of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushUpdate {
    /// The commit the branch is to name.
    pub new: GitOid,
    /// The remote branch, by its short name.
    pub branch: String,
    /// The commit the branch must name now; `None`: it must not exist.
    pub expect: Option<GitOid>,
}

/// What a push that succeeded did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The remote moved.
    Sent,
    /// The remote held the commit already (git's porcelain `=`): nothing was sent.
    AlreadyThere,
}

/// What a push would add to a remote: each blob with its size and path, and the bytes on disk
/// of every object it adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewObjects {
    /// Each blob: its id, its size in bytes, and a path it is at.
    pub blobs: Vec<(GitOid, u64, String)>,
    /// The bytes on disk of the objects the push adds: an upper bound of the pack.
    pub disk_usage: u64,
}

/// Why git failed, as the classifier reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// No git to run (`E_GIT_UNSUPPORTED`, `git_missing`).
    GitMissing,
    /// A git older than 2.39 (`E_GIT_UNSUPPORTED`, `git_too_old`).
    GitTooOld {
        /// The version it reported.
        found: String,
    },
    /// The network or the host did not answer (`E_REMOTE_UNREACHABLE`).
    Unreachable {
        /// What failed.
        cause: Cause,
        /// The end of git's stderr, redacted.
        stderr_tail: String,
    },
    /// The remote or git refused (`E_REMOTE_REJECTED`).
    Rejected {
        /// Why.
        reason: RejectReason,
        /// Each refused ref and the reason the remote gave.
        refs: Vec<(String, String)>,
        /// The remote's own `remote:` lines, redacted.
        remote_messages: Vec<String>,
        /// git's exit status, when it exited.
        git_exit: Option<i32>,
        /// The end of git's stderr, redacted.
        stderr_tail: String,
        /// Whether the remote names a token variable that is not set.
        token_env_unset: bool,
    },
    /// The push's lease failed: the branch moved since the last fetch (`E_REMOTE_DIVERGED`,
    /// `remote_moved`).
    Stale {
        /// The remote branch.
        branch: String,
    },
    /// The remote refused a push that is not a fast-forward (`E_REMOTE_DIVERGED`,
    /// `not_fast_forward`).
    NotFastForward {
        /// The remote branch.
        branch: String,
    },
    /// The remote is a SHA-256 repository (`E_GIT_UNSUPPORTED`, `sha256`).
    Sha256,
}

/// What did not answer, as `details.cause` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The host's name did not resolve: `dns`.
    Dns,
    /// The connection was refused or reset: `connect`.
    Connect,
    /// The connection or the transfer timed out: `timeout`.
    Timeout,
    /// The TLS handshake failed: `tls`.
    Tls,
    /// The remote hung up early: `disconnected`.
    Disconnected,
    /// ssh could not reach the host: `ssh`.
    Ssh,
}

impl Cause {
    /// The cause as `details.cause` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::Dns => "dns",
            Cause::Connect => "connect",
            Cause::Timeout => "timeout",
            Cause::Tls => "tls",
            Cause::Disconnected => "disconnected",
            Cause::Ssh => "ssh",
        }
    }
}

/// Why the remote or git refused, as `details.reason` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The credential was refused or missing: `auth`.
    Auth,
    /// No such repository, or none the credential can see: `not_found`.
    NotFound,
    /// A hook or a rule of the remote declined the push: `declined`.
    Declined,
    /// A failure git's messages do not explain: `unclassified`.
    Unclassified,
}

impl RejectReason {
    /// The reason as `details.reason` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::Auth => "auth",
            RejectReason::NotFound => "not_found",
            RejectReason::Declined => "declined",
            RejectReason::Unclassified => "unclassified",
        }
    }
}

impl UrlRefused {
    /// The reason as `details.reason` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            UrlRefused::CleartextHttp => "cleartext_http",
            UrlRefused::Scheme => "url_scheme",
            UrlRefused::CredentialInUrl => "credential_in_url",
            UrlRefused::OptionLike => "url_option",
        }
    }
}
