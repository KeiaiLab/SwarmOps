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
//! Failures are classified by one table (`transport/classify.rs`); every one leaves the store
//! as it was.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Write as _};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread;

use base64::Engine as _;

use crate::oid::GitOid;

mod classify;

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
#[derive(Clone)]
pub struct GitCli {
    /// The binary: `SWARMOPS_GIT`, else `git` as `PATH` finds it.
    binary: PathBuf,
    /// The caller's environment, which each child inherits but for what the rules drop.
    env: Vec<(OsString, OsString)>,
}

impl GitCli {
    /// Locates the binary; fails only with [`TransportError::GitMissing`]. Its version is read
    /// by the first command it runs, so a push that sends nothing runs no git.
    pub fn find() -> Result<GitCli, TransportError> {
        GitCli::find_with_env(std::env::vars_os())
    }

    /// [`GitCli::find`] in the environment `env` instead of the process's: the binary is
    /// looked up in it, the children inherit it, and a remote's token variable is read from it.
    pub fn find_with_env<I>(env: I) -> Result<GitCli, TransportError>
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let env: Vec<(OsString, OsString)> = env.into_iter().collect();
        let binary = locate(&env).ok_or(TransportError::GitMissing)?;
        Ok(GitCli { binary, env })
    }

    /// The binary this runs.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The caller's value of `key`, if set.
    fn var(&self, key: &str) -> Option<&OsStr> {
        lookup(&self.env, key)
    }

    /// The one place every child is built: the version checked (once per process and binary),
    /// the environment cleaned and set, the configuration passed in it, stdin closed. `args`
    /// already end in `--end-of-options` and the values, where values follow.
    fn command(&self, call: &Call<'_>, args: &[OsString]) -> Result<Child, TransportError> {
        self.check_version()?;
        let mut command = self.base_command();
        command.args(args);

        // 1. The mirror: git's repository, and hooks from an empty directory. Its path, given
        //    and canonical, is redacted like a token: git names it in errors (a held ref lock).
        let mut secrets = Secrets::default();
        let mut config: Vec<(String, OsString)> = CONFIG
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
            .collect();
        if let Some(mirror) = call.mirror {
            command.env("GIT_DIR", mirror).current_dir(mirror);
            config.push(("core.hooksPath".to_owned(), mirror.join(HOOKS_NONE).into()));
            secrets.add(mirror.to_string_lossy().into_owned());
            if let Ok(canonical) = mirror.canonicalize() {
                secrets.add(canonical.to_string_lossy().into_owned());
            }
        }

        // 2. The transports git may use: the remote's scheme alone, or none.
        let allowed = call.remote.map_or("", |(url, _)| url.protocol.as_str());
        command.env("GIT_ALLOW_PROTOCOL", allowed);

        // 3. The remote's token, as a header scoped to its URL (a redirect elsewhere drops it).
        let remote_token = call
            .remote
            .and_then(|(url, cred)| Some((url, self.token(cred)?)));
        if let Some((url, token)) = remote_token {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{TOKEN_USER}:{token}"));
            let header = format!("Authorization: Basic {encoded}");
            let key = format!("http.{}.extraHeader", url.as_str());
            config.push((key, header.clone().into()));
            secrets.add(header);
            secrets.add(encoded);
            secrets.add(token);
        }

        // 4. The configuration, in the environment (never in the arguments, which others see).
        command.env("GIT_CONFIG_COUNT", config.len().to_string());
        for (n, (key, value)) in config.into_iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{n}"), key)
                .env(format!("GIT_CONFIG_VALUE_{n}"), value);
        }
        Ok(Child { command, secrets })
    }

    /// git with the cleaned environment, stdin closed, and nothing of a call's own.
    fn base_command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env_clear()
            .envs(self.env.iter().filter(|(key, _)| kept(key)).cloned())
            .envs(FIXED_ENV)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null());
        command
    }

    /// The token `cred` names, when its variable is set, not empty and UTF-8.
    fn token(&self, cred: &Credential) -> Option<String> {
        let name = cred.token_env.as_deref()?;
        let value = self.var(name)?.to_str()?;
        (!value.is_empty()).then(|| value.to_owned())
    }

    /// Whether `cred` names a token variable that holds no token.
    fn token_env_unset(&self, cred: &Credential) -> bool {
        cred.token_env.is_some() && self.token(cred).is_none()
    }

    /// Refuses a git older than [`MIN_GIT`], by `git version`, read once per process for each
    /// binary.
    fn check_version(&self) -> Result<(), TransportError> {
        static CHECKED: Mutex<BTreeMap<PathBuf, Result<(), TransportError>>> =
            Mutex::new(BTreeMap::new());
        let mut checked = CHECKED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(known) = checked.get(&self.binary) {
            return known.clone();
        }
        let found = self.read_version();
        checked.insert(self.binary.clone(), found.clone());
        found
    }

    /// `git version`, against [`MIN_GIT`].
    fn read_version(&self) -> Result<(), TransportError> {
        let mut command = self.base_command();
        command.arg("version");
        let ran = run(&mut command, &Input::Null)?;
        if !ran.status.success() {
            return Err(TransportError::GitMissing);
        }

        // `git version 2.43.0`, or `git version 2.39.3 (Apple Git-146)`.
        let line = String::from_utf8_lossy(&ran.stdout);
        let found = line
            .trim()
            .strip_prefix("git version ")
            .unwrap_or(line.trim())
            .to_owned();
        match release(&found) {
            Some(release) if release >= MIN_GIT => Ok(()),
            _ => Err(TransportError::GitTooOld { found }),
        }
    }

    /// Runs `args` for `call` with `input` on stdin; a non-zero exit is classified.
    fn git(
        &self,
        call: &Call<'_>,
        args: &[OsString],
        input: &Input,
    ) -> Result<Ran, TransportError> {
        let ran = self.spawn(call, args, input)?;
        if ran.status.success() {
            return Ok(ran);
        }
        Err(self.failure(call, &ran))
    }

    /// Runs `args` for `call` with `input` on stdin, whatever its exit; its output redacted.
    fn spawn(
        &self,
        call: &Call<'_>,
        args: &[OsString],
        input: &Input,
    ) -> Result<Ran, TransportError> {
        let Child {
            mut command,
            secrets,
        } = self.command(call, args)?;
        let mut ran = run(&mut command, input)?;
        ran.stderr = secrets.redact(&ran.stderr);
        ran.porcelain = secrets.redact(&String::from_utf8_lossy(&ran.stdout));
        Ok(ran)
    }

    /// What git's failure in `ran` was, for `call`.
    fn failure(&self, call: &Call<'_>, ran: &Ran) -> TransportError {
        classify::failure(&self.said(call, ran))
    }

    /// What git said in `ran`, for the classifier.
    fn said<'a>(&self, call: &Call<'_>, ran: &'a Ran) -> classify::Said<'a> {
        classify::Said {
            exit: ran.status.code(),
            stderr: &ran.stderr,
            token_env_unset: call
                .remote
                .is_some_and(|(_, cred)| self.token_env_unset(cred)),
        }
    }
}

impl fmt::Debug for GitCli {
    // The environment may hold tokens: only the binary is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitCli")
            .field("binary", &self.binary)
            .finish_non_exhaustive()
    }
}

impl Transport for GitCli {
    fn ls_remote_head(
        &self,
        url: &RemoteUrl,
        cred: &Credential,
    ) -> Result<Option<String>, TransportError> {
        let call = Call {
            mirror: None,
            remote: Some((url, cred)),
        };
        let args = args(
            &["ls-remote", "--symref", END_OF_OPTIONS],
            [OsString::from(url.as_str()), OsString::from("HEAD")],
        );
        let ran = self.git(&call, &args, &Input::Null)?;

        // `ref: refs/heads/main<TAB>HEAD`: the symbolic ref, when the remote names one.
        let head = ran.porcelain.lines().find_map(|line| {
            let target = line.strip_prefix("ref: ")?.strip_suffix("\tHEAD")?;
            target.strip_prefix(HEADS).map(str::to_owned)
        });
        Ok(head)
    }

    fn fetch(
        &self,
        mirror: &Path,
        url: &RemoteUrl,
        cred: &Credential,
        refspecs: &[RefSpec],
    ) -> Result<(), TransportError> {
        let call = Call {
            mirror: Some(mirror),
            remote: Some((url, cred)),
        };

        // The mirror records what the remote holds: each refspec forced (`+`).
        let values = std::iter::once(OsString::from(url.as_str())).chain(
            refspecs
                .iter()
                .map(|spec| OsString::from(format!("+{}:{}", spec.src, spec.dst))),
        );
        let args = args(&FETCH_FLAGS, values);
        self.git(&call, &args, &Input::Null)?;
        Ok(())
    }

    fn push(
        &self,
        mirror: &Path,
        url: &RemoteUrl,
        cred: &Credential,
        update: &PushUpdate,
    ) -> Result<PushOutcome, TransportError> {
        let call = Call {
            mirror: Some(mirror),
            remote: Some((url, cred)),
        };

        // 1. The lease (the branch's expected commit, empty: absent) and one refspec, never `+`.
        let dst = format!("{HEADS}{}", update.branch);
        let expect = update.expect.map(|oid| oid.to_string()).unwrap_or_default();
        let mut argv: Vec<OsString> = PUSH_FLAGS.iter().map(OsString::from).collect();
        argv.extend([
            OsString::from(format!("--force-with-lease={dst}:{expect}")),
            OsString::from(END_OF_OPTIONS),
            OsString::from(url.as_str()),
            OsString::from(format!("{}:{dst}", update.new)),
        ]);

        // 2. The outcome, from the porcelain line of the branch.
        let ran = self.spawn(&call, &argv, &Input::Null)?;
        let said = self.said(&call, &ran);
        classify::pushed(&ran.porcelain, &dst, &update.branch, &said)
    }

    fn new_objects(
        &self,
        mirror: &Path,
        new: GitOid,
        old: Option<GitOid>,
    ) -> Result<NewObjects, TransportError> {
        let call = Call {
            mirror: Some(mirror),
            remote: None,
        };
        let range: Vec<OsString> = std::iter::once(new.to_string())
            .chain(old.map(|old| format!("^{old}")))
            .map(OsString::from)
            .collect();

        // 1. The objects `new` adds, each tree and blob with a path.
        let listed = self.git(
            &call,
            &args(&["rev-list", "--objects", END_OF_OPTIONS], range.clone()),
            &Input::Null,
        )?;
        let named: BTreeMap<GitOid, String> = listed
            .porcelain
            .lines()
            .filter_map(|line| {
                let (hex, path) = line.split_once(' ')?;
                Some((GitOid::from_hex(hex.as_bytes())?, path.to_owned()))
            })
            .collect();

        // 2. Their types and sizes: cat-file reads the ids on a pipe, one 40-hex line each.
        let mut blobs = Vec::new();
        if !named.is_empty() {
            let lines = named.keys().map(GitOid::to_string).collect();
            let sized = self.git(
                &call,
                &args(&["cat-file", "--batch-check"], []),
                &Input::Lines(lines),
            )?;
            blobs = sized
                .porcelain
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split(' ');
                    let id = GitOid::from_hex(fields.next()?.as_bytes())?;
                    let size = match fields.next()? {
                        "blob" => fields.next()?.parse().ok()?,
                        _ => return None,
                    };
                    Some((id, size, named.get(&id)?.clone()))
                })
                .collect();
        }

        // 3. Their bytes on disk together.
        let usage = self.git(
            &call,
            &args(
                &["rev-list", "--objects", "--disk-usage", END_OF_OPTIONS],
                range,
            ),
            &Input::Null,
        )?;
        let disk_usage = usage.porcelain.trim().parse().unwrap_or(0);
        Ok(NewObjects { blobs, disk_usage })
    }

    fn gc_auto(&self, mirror: &Path) -> Result<(), TransportError> {
        let call = Call {
            mirror: Some(mirror),
            remote: None,
        };
        self.git(&call, &args(&["gc", "--auto", "--quiet"], []), &Input::Null)?;
        Ok(())
    }

    fn is_ancestor(
        &self,
        mirror: &Path,
        ancestor: GitOid,
        descendant: GitOid,
    ) -> Result<bool, TransportError> {
        let call = Call {
            mirror: Some(mirror),
            remote: None,
        };
        let args = args(
            &["merge-base", "--is-ancestor", END_OF_OPTIONS],
            [ancestor, descendant].map(|oid| OsString::from(oid.to_string())),
        );

        // Exit 0: an ancestor; 1: not one; anything else: git failed.
        let ran = self.spawn(&call, &args, &Input::Null)?;
        match ran.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self.failure(&call, &ran)),
        }
    }
}

/// A remote's URL that the rules accept: `https://`, `ssh://` or the scp form, and plain
/// `http://` to a loopback address only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUrl {
    /// The URL, as given.
    url: String,
    /// The host it names, without user, port or brackets.
    host: String,
    /// The transport git uses for it.
    protocol: Protocol,
}

impl RemoteUrl {
    /// `url`, checked against the rules.
    pub fn parse(url: &str) -> Result<RemoteUrl, UrlRefused> {
        // 1. Nothing git could read as an option, and no control character.
        if url.starts_with('-') {
            return Err(UrlRefused::OptionLike);
        }
        if url.chars().any(char::is_control) {
            return Err(UrlRefused::Scheme);
        }

        // 2. `<scheme>://…`, or the scp form `[user@]host:path`.
        let (protocol, user, host) = if let Some((scheme, rest)) = url.split_once("://") {
            let protocol = Protocol::of(scheme).ok_or(UrlRefused::Scheme)?;
            let authority = rest.split('/').next().unwrap_or_default();
            let (user, host_port) = split_user(authority);
            (protocol, user, host_of(host_port))
        } else {
            let (user, host) = scp_parts(url).ok_or(UrlRefused::Scheme)?;
            (Protocol::Ssh, user, host)
        };

        // 3. The user and host: neither an option, no password, a host at all.
        if user.is_some_and(|user| user.starts_with('-')) || host.starts_with('-') {
            return Err(UrlRefused::OptionLike);
        }
        if user.is_some_and(|user| user.contains(':')) {
            return Err(UrlRefused::CredentialInUrl);
        }
        if host.is_empty() {
            return Err(UrlRefused::Scheme);
        }

        // 4. Plain HTTP to a literal loopback address only.
        if protocol == Protocol::Http && !is_loopback(host) {
            return Err(UrlRefused::CleartextHttp);
        }
        Ok(RemoteUrl {
            url: url.to_owned(),
            host: host.to_owned(),
            protocol,
        })
    }

    /// The host the URL names, without user or port.
    pub fn host(&self) -> &str {
        &self.host
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

/// The oldest git the transport runs, `[major, minor]`.
const MIN_GIT: [u32; 2] = [2, 39];

/// The variable that names the git binary, an absolute path.
const GIT_ENV: &str = "SWARMOPS_GIT";

/// The binary's file name on `PATH`.
const GIT_NAME: &str = if cfg!(windows) { "git.exe" } else { "git" };

/// What ends the options of every command that takes values after them.
const END_OF_OPTIONS: &str = "--end-of-options";

/// Where a remote's branches are, in its ref names.
const HEADS: &str = "refs/heads/";

/// The user name a token goes with over HTTPS (GitHub's for an installation token; a personal
/// token takes any).
const TOKEN_USER: &str = "x-access-token";

/// The mirror's directory of hooks, which does not exist: no hook runs.
const HOOKS_NONE: &str = "hooks-none";

/// The bytes of git's stderr kept while it runs: its end.
const STDERR_KEPT: usize = 64 << 10;

/// The variables `git rev-parse --local-env-vars` lists (git 2.43): each points git at another
/// repository, so none reaches a child.
const LOCAL_ENV_VARS: [&str; 16] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// Other variables no child gets: traces print headers, the token's among them, to the stderr
/// errors report; the locale is set to `C`, which the classifier reads.
const DROPPED_VARS: [&str; 5] = [
    "GIT_CURL_VERBOSE",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_MESSAGES",
];

/// Prefixes of the variables no child gets: every trace, and configuration entries the caller
/// set beside the dropped `GIT_CONFIG_COUNT`.
const DROPPED_PREFIXES: [&str; 3] = ["GIT_TRACE", "GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// What every child's environment sets: git's messages in English, no prompt (a missing
/// credential fails), and a stalled HTTP transfer ended after a minute below 1 KB/s.
const FIXED_ENV: [(&str, &str); 4] = [
    ("LC_ALL", "C"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_HTTP_LOW_SPEED_LIMIT", "1000"),
    ("GIT_HTTP_LOW_SPEED_TIME", "60"),
];

/// The configuration every command gets in its environment: git's writes fsynced, its
/// housekeeping in the foreground, protocol v2, no submodule, no credential in a URL.
const CONFIG: [(&str, &str); 6] = [
    ("core.fsync", "all"),
    ("core.fsyncMethod", "fsync"),
    ("gc.autoDetach", "false"),
    ("protocol.version", "2"),
    ("fetch.recurseSubmodules", "false"),
    ("transfer.credentialsInUrl", "die"),
];

/// `git fetch`'s fixed options, up to the URL.
const FETCH_FLAGS: [&str; 8] = [
    "fetch",
    "--no-tags",
    "--prune",
    "--no-write-fetch-head",
    "--no-auto-gc",
    "--no-recurse-submodules",
    "--no-show-forced-updates",
    END_OF_OPTIONS,
];

/// `git push`'s fixed options, before the lease: never `--force`.
const PUSH_FLAGS: [&str; 5] = [
    "push",
    "--porcelain",
    "--no-verify",
    "--no-follow-tags",
    "--no-signed",
];

/// The transport git uses for a remote, as `GIT_ALLOW_PROTOCOL` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    /// `https://`.
    Https,
    /// `ssh://` and the scp form.
    Ssh,
    /// `http://`, to loopback only.
    Http,
}

impl Protocol {
    /// The protocol of `scheme`, among those a remote may have.
    fn of(scheme: &str) -> Option<Protocol> {
        match scheme {
            "https" => Some(Protocol::Https),
            "ssh" => Some(Protocol::Ssh),
            "http" => Some(Protocol::Http),
            _ => None,
        }
    }

    /// Its name in `GIT_ALLOW_PROTOCOL`.
    fn as_str(self) -> &'static str {
        match self {
            Protocol::Https => "https",
            Protocol::Ssh => "ssh",
            Protocol::Http => "http",
        }
    }
}

/// What a command works on: the mirror, and the remote with its credential.
struct Call<'a> {
    /// The mirror, `GIT_DIR`; `None` for `ls-remote`.
    mirror: Option<&'a Path>,
    /// The remote it talks to; `None` for the mirror's own commands.
    remote: Option<(&'a RemoteUrl, &'a Credential)>,
}

/// A command ready to run, and what its output must not show.
struct Child {
    /// The command.
    command: Command,
    /// The token's forms.
    secrets: Secrets,
}

/// What is replaced in git's output: the token's forms (the header line, its base64 and the
/// token) and the mirror's path, given and canonical; longest first.
#[derive(Default)]
struct Secrets(Vec<String>);

impl Secrets {
    /// Adds `secret`, keeping the longest first, so that no shorter form cuts a longer one.
    fn add(&mut self, secret: String) {
        let at = self.0.partition_point(|kept| kept.len() >= secret.len());
        self.0.insert(at, secret);
    }

    /// `text` with every form replaced by `<redacted>`.
    fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in self.0.iter().filter(|secret| !secret.is_empty()) {
            text = text.replace(secret.as_str(), "<redacted>");
        }
        text
    }
}

/// A child's stdin.
enum Input {
    /// None (`Stdio::null()`).
    Null,
    /// A pipe holding these lines, each a 40-hex object id SwarmOps wrote.
    Lines(Vec<String>),
}

/// What a child did.
struct Ran {
    /// Its exit.
    status: ExitStatus,
    /// Its stdout, raw.
    stdout: Vec<u8>,
    /// Its stdout as text, redacted (push's porcelain lines, rev-list's ids).
    porcelain: String,
    /// The end of its stderr ([`STDERR_KEPT`]), redacted.
    stderr: String,
}

/// Runs `command` with `input` on stdin, capturing stdout and the end of stderr.
fn run(command: &mut Command, input: &Input) -> Result<Ran, TransportError> {
    if matches!(input, Input::Lines(_)) {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| TransportError::GitMissing)?;

    // 1. stdin written, stdout and stderr read, at once (a full pipe would stall git).
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let (out, err) = thread::scope(|scope| {
        if let (Some(mut stdin), Input::Lines(lines)) = (stdin, input) {
            scope.spawn(move || {
                for line in lines {
                    if writeln!(stdin, "{line}").is_err() {
                        break;
                    }
                }
            });
        }
        let reader = scope.spawn(move || {
            let mut out = Vec::new();
            if let Some(stdout) = stdout.as_mut() {
                let _ = stdout.read_to_end(&mut out);
            }
            out
        });
        let err = stderr.as_mut().map(read_end).unwrap_or_default();
        (reader.join().unwrap_or_default(), err)
    });

    // 2. Its exit.
    let status = child.wait().map_err(|_| TransportError::GitMissing)?;
    Ok(Ran {
        status,
        stdout: out,
        porcelain: String::new(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

/// The last [`STDERR_KEPT`] bytes `reader` gives, from a line's start when the front was cut.
fn read_end(reader: &mut impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut cut = false;
    let mut chunk = [0; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        kept.extend_from_slice(&chunk[..read]);

        // Dropped from the front in large steps, so that keeping the end stays linear.
        if kept.len() > 2 * STDERR_KEPT {
            kept.drain(..kept.len() - STDERR_KEPT);
            cut = true;
        }
    }
    if kept.len() > STDERR_KEPT {
        kept.drain(..kept.len() - STDERR_KEPT);
        cut = true;
    }
    if !cut {
        return kept;
    }

    // The first line, which may hold part of a token redaction cannot see, goes too.
    let first_line = kept
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(kept.len(), |at| at + 1);
    kept.drain(..first_line);
    kept
}

/// `fixed` then `values`, as one argument list.
fn args<I>(fixed: &[&str], values: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    fixed.iter().map(OsString::from).chain(values).collect()
}

/// The value of `key` in `env`.
fn lookup<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsStr> {
    env.iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_os_str())
}

/// The binary: [`GIT_ENV`]'s absolute path when set, else the first [`GIT_NAME`] on `PATH`.
fn locate(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    if let Some(named) = lookup(env, GIT_ENV).filter(|named| !named.is_empty()) {
        let named = PathBuf::from(named);
        return (named.is_absolute() && named.is_file()).then_some(named);
    }
    let path = lookup(env, "PATH")?;
    std::env::split_paths(path)
        .map(|dir| dir.join(GIT_NAME))
        .find(|candidate| candidate.is_file())
}

/// Whether a child inherits the caller's variable `key`.
fn kept(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return true;
    };
    !(LOCAL_ENV_VARS.contains(&key)
        || DROPPED_VARS.contains(&key)
        || DROPPED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix)))
}

/// The `[major, minor]` `text` starts with: `2.43.0`, `2.39.3 (Apple Git-146)`, `2.45.0.rc1`.
fn release(text: &str) -> Option<[u32; 2]> {
    let mut numbers = text.split(['.', ' ']).map(str::parse::<u32>);
    Some([numbers.next()?.ok()?, numbers.next()?.ok()?])
}

/// An authority's user (before its last `@`) and the rest.
fn split_user(authority: &str) -> (Option<&str>, &str) {
    match authority.rsplit_once('@') {
        Some((user, rest)) => (Some(user), rest),
        None => (None, authority),
    }
}

/// The host of `host[:port]` or `[v6][:port]`, without brackets or port.
fn host_of(host_port: &str) -> &str {
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or_default();
    }
    host_port.split(':').next().unwrap_or_default()
}

/// The user and host of the scp form `[user@]host:path`: a `:` before any `/`, and not git's
/// `<transport>::<address>`. `None` for anything else (a local path among it).
fn scp_parts(url: &str) -> Option<(Option<&str>, &str)> {
    if url.contains("::") {
        return None;
    }
    let (before, _path) = url.split_once(':')?;
    if before.contains('/') {
        return None;
    }
    Some(split_user(before))
}

/// Whether `host` is a literal loopback address: `127.0.0.0/8` or `::1`.
fn is_loopback(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}
