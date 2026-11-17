//! The login file: one file of the user's, outside every store, that holds a token per host.
//!
//! # Where
//!
//! `SWARMOPS_AUTH_FILE` names it, by an absolute path; otherwise it is `swarmops/auth.toml` in
//! the host's state directory ([`userdir`]), beside the memo keys and the workspaces. A host that
//! names neither has no login file: there are no logins to list or to remove, and a login is
//! refused (`details.reason` `auth_dir`). No store holds the file, names it or is read for it.
//!
//! # Format
//!
//! ```toml
//! # SwarmOps logins. The tokens are in plain text: do not copy, share or commit this file.
//! version = 1
//!
//! [[login]]
//! host = "github.com"
//! provider = "gh"
//! source = "with_token"
//! token = "<the token>"
//! ```
//!
//! `version` is read first, and a file of another version is refused, never rewritten. `host` is
//! the key a login is looked up by: a [`host_key`], so `host` or `host:port` in lower case, once
//! in the file. `provider` is an open string: `gh` is the one this build knows ([`Provider`]). An
//! entry of another provider needs only its `provider` and its `host`; it is listed, and its
//! token is never used. `source` says how the token was obtained and `scopes`, a list of strings,
//! what the provider granted; a token read from stdin has no known scopes.
//!
//! Unlike the store's `remotes.toml`, a rewrite keeps what this build does not know: every
//! top-level key and table, every key of an entry it does not replace, and every entry of
//! another provider, as read. An older build must not delete a newer one's login. Comments are
//! not kept: the file is written whole, under its one header line.
//!
//! # Reading and writing
//!
//! The file is the user's alone, and a read checks that on the file it reads: a symbolic link
//! is refused, and the kind, the mode (no bit for the group or for others) and the size (at most
//! 64 KiB) are those of the open file the bytes then come from, not of whatever the path names a
//! moment later. Nothing loosens or tightens a file's mode for the user: a loose file is refused
//! with the `chmod` that fixes it. A file that is refused is never overwritten.
//!
//! A file this build does not read is `E_INVALID_ARG`, with the file in `details.auth_file` and
//! the reason in `details.reason`:
//!
//! | `details.reason` | The file |
//! | --- | --- |
//! | `auth_file_kind` | is a symbolic link, or not a regular file |
//! | `auth_file_mode` | can be read or written by its group or by others |
//! | `auth_file_version` | names no `version`, or not this build's |
//! | `auth_file` | holds more than 64 KiB, is not UTF-8 or not TOML, or holds an entry that is not a login: no `provider` or `host`, a host met twice, or, for a provider this build knows, a host that is no [`host_key`], a token that is none, or a `source` or `scopes` of another shape |
//!
//! A file that cannot be read at all is `E_IO`. No message quotes the file's content.
//!
//! A write makes the directory with mode 0700 when it is missing and replaces the file whole: a
//! temp file of mode 0600 beside it, synced and renamed over it. No lock is taken, so of two
//! logins at once the later one's file stands, whole and valid.
//!
//! The token appears in no error, no result and no `Debug` output of this module: the results
//! are the API's types, which have no field for it.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::os::unix::fs::MetadataExt as _;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use swarmops_api::{AuthEntry, AuthLogin, AuthLogout, AuthStatus, ErrorCode, OpError};
use swarmops_git::transport::host_key;
use swarmops_store::{FsOps as _, RealFs};

use crate::{error, identity, userdir};

/// The environment variable that names the login file, by an absolute path.
pub const AUTH_FILE_ENV: &str = "SWARMOPS_AUTH_FILE";

/// The login file's name in the host's state directory, under `swarmops/`.
const FILE_NAME: &str = "auth.toml";

/// The one version of the file this build reads and writes.
const VERSION: i64 = 1;

/// The most bytes a login file may hold.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The most bytes a token may hold.
const MAX_TOKEN_BYTES: usize = 1024;

/// The mode bits a login file must not have: any of the group's or of others'.
const LOOSE_MODE: u32 = 0o077;

/// The file's first line.
const HEADER: &str =
    "# SwarmOps logins. The tokens are in plain text: do not copy, share or commit this file.\n";

/// The file's keys: the version, the array of logins, and an entry's own.
const VERSION_KEY: &str = "version";
const LOGINS_KEY: &str = "login";
const PROVIDER_KEY: &str = "provider";
const HOST_KEY: &str = "host";
const TOKEN_KEY: &str = "token";
const SOURCE_KEY: &str = "source";
const SCOPES_KEY: &str = "scopes";

/// The hint of a file this build cannot read as a login file.
const START_OVER_HINT: &str = "remove the file to start over, then log in again";

/// A provider this build can log in to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Provider {
    /// GitHub, at `github.com`.
    Gh,
}

impl Provider {
    /// The provider's name in the file and in a result: `gh`.
    pub const fn key(self) -> &'static str {
        match self {
            Provider::Gh => "gh",
        }
    }

    /// The host a login with the provider is for, as a [`host_key`]: `github.com`.
    pub const fn host(self) -> &'static str {
        match self {
            Provider::Gh => "github.com",
        }
    }

    /// The provider the file names `key`, if this build knows it.
    fn named(key: &str) -> Option<Provider> {
        [Provider::Gh]
            .into_iter()
            .find(|provider| provider.key() == key)
    }
}

/// How a login's token was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Source {
    /// Read from stdin (`auth login --with-token`).
    WithToken,
    /// Granted by the provider's device flow.
    Device,
}

impl Source {
    /// The source's name in the file and in a result.
    pub const fn key(self) -> &'static str {
        match self {
            Source::WithToken => "with_token",
            Source::Device => "device",
        }
    }
}

/// The login file's path, as `env` names it (the module's "Where"): `None` on a host that names
/// no directory for it. A variable set to the empty string counts as unset; one that is not
/// UTF-8, or a `SWARMOPS_AUTH_FILE` that is not an absolute path, is `E_INVALID_ARG`, said of the
/// variable (`details.source`).
pub fn file(env: &dyn Fn(&str) -> Option<OsString>) -> Result<Option<Utf8PathBuf>, OpError> {
    if let Some(named) = identity::var(&env, AUTH_FILE_ENV)? {
        let path = Utf8PathBuf::from(named);
        if !path.is_absolute() {
            let refused = error::error(
                ErrorCode::InvalidArg,
                format!("{AUTH_FILE_ENV}: the login file's path is not absolute"),
            );
            return Err(refused.with_details(error::details([("source", AUTH_FILE_ENV.into())])));
        }
        return Ok(Some(path));
    }
    let state = userdir::state_dir(env)?;
    Ok(state.map(|state| state.join("swarmops").join(FILE_NAME)))
}

/// `auth login`: keeps `token` as the login with `provider` in the login file `file`, replacing
/// the one the file holds for the provider's host, if any (`replaced`). `source` and `scopes`
/// are recorded with it.
///
/// `E_INVALID_ARG` for a token that is not 1 to 1024 visible ASCII characters
/// (`details.reason` `token`), a host with no login file (`auth_dir`), a file this build does
/// not read (`auth_file_kind`, `auth_file_mode`, `auth_file_version` or `auth_file`), and a file
/// whose login for that host is another provider's (`provider_mismatch`); nothing is written
/// then. `E_IO` when the file cannot be read or written.
pub fn login(
    file: Option<&Utf8Path>,
    provider: Provider,
    source: Source,
    scopes: Option<&[String]>,
    token: &str,
) -> Result<AuthLogin, OpError> {
    // 1. A token that is one, and a file to keep it in.
    check_token(token)?;
    let Some(file) = file else {
        let refused = error::error(
            ErrorCode::InvalidArg,
            format!("no directory for the login file: set {AUTH_FILE_ENV}"),
        );
        return Err(refused.with_details(error::details([("reason", "auth_dir".into())])));
    };
    let mut logins = Logins::read(file)?;

    // 2. The entry, in the place of the one it replaces.
    let mut entry = toml::Table::new();
    entry.insert(PROVIDER_KEY.to_owned(), provider.key().into());
    entry.insert(HOST_KEY.to_owned(), provider.host().into());
    entry.insert(SOURCE_KEY.to_owned(), source.key().into());
    if let Some(scopes) = scopes {
        entry.insert(SCOPES_KEY.to_owned(), scopes.to_vec().into());
    }
    entry.insert(TOKEN_KEY.to_owned(), token.into());
    let at = logins.position(provider)?;
    let entries = logins.entries_mut();
    match at {
        Some(at) => entries[at] = toml::Value::Table(entry),
        None => entries.push(toml::Value::Table(entry)),
    }
    logins.write()?;

    // 3. What was kept, without the token.
    let mut kept = AuthEntry::new(provider.key(), provider.host());
    kept.source = Some(source.key().to_owned());
    kept.scopes = scopes.map(<[String]>::to_vec);
    Ok(AuthLogin::new(kept, file.as_str(), at.is_some()))
}

/// `auth status`: the logins the login file `file` holds, in its order, those of providers this
/// build does not know among them; none when there is no file. Offline: no token is checked.
///
/// `E_INVALID_ARG` for a file this build does not read (`details.reason` `auth_file_kind`,
/// `auth_file_mode`, `auth_file_version` or `auth_file`); `E_IO` when it cannot be read.
pub fn status(file: Option<&Utf8Path>) -> Result<AuthStatus, OpError> {
    let Some(file) = file else {
        return Ok(AuthStatus::new(Vec::new()));
    };
    let logins = Logins::read(file)?;
    let mut status = AuthStatus::new(logins.listed);
    status.file = Some(file.as_str().to_owned());
    Ok(status)
}

/// `auth logout`: removes the login with `provider` from the login file `file`; `removed` is
/// false, and nothing is written, when the file holds none. The token is not revoked at the
/// provider.
///
/// `E_INVALID_ARG` for a file this build does not read (`details.reason` `auth_file_kind`,
/// `auth_file_mode`, `auth_file_version` or `auth_file`) and for a file whose login for the
/// provider's host is another provider's (`provider_mismatch`).
/// `E_IO` when the file cannot be written.
pub fn logout(file: Option<&Utf8Path>, provider: Provider) -> Result<AuthLogout, OpError> {
    let Some(file) = file else {
        return Ok(AuthLogout::new(provider.key(), provider.host(), false));
    };
    let mut logins = Logins::read(file)?;
    let at = logins.position(provider)?;
    if let Some(at) = at {
        logins.entries_mut().remove(at);
        logins.write()?;
    }
    let mut out = AuthLogout::new(provider.key(), provider.host(), at.is_some());
    out.file = Some(file.as_str().to_owned());
    Ok(out)
}

/// Refuses a token that is not 1 to [`MAX_TOKEN_BYTES`] visible ASCII characters
/// (`E_INVALID_ARG`, `details.reason` `token`). The message says which rule it broke and never
/// holds the token.
fn check_token(token: &str) -> Result<(), OpError> {
    let why = if token.is_empty() {
        "is empty".to_owned()
    } else if token.len() > MAX_TOKEN_BYTES {
        format!("is longer than {MAX_TOKEN_BYTES} bytes")
    } else if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        "holds a space, a control character or a character outside ASCII".to_owned()
    } else {
        return Ok(());
    };
    let refused = error::error(ErrorCode::InvalidArg, format!("the token {why}"));
    Err(refused.with_details(error::details([("reason", "token".into())])))
}

/// A login file as read: its whole content, and its logins without their tokens.
struct Logins {
    /// The file's path.
    path: Utf8PathBuf,
    /// Everything the file holds, the keys this build does not know among them.
    table: toml::Table,
    /// The file's logins, in its order.
    listed: Vec<AuthEntry>,
}

impl Logins {
    /// The login file at `path`, checked: no logins when there is no file. A file this build
    /// does not read is refused as [the module's table](self) says.
    fn read(path: &Utf8Path) -> Result<Logins, OpError> {
        let mut logins = Logins {
            path: path.to_owned(),
            table: toml::Table::new(),
            listed: Vec::new(),
        };
        let Some(bytes) = read_private(path)? else {
            logins
                .table
                .insert(VERSION_KEY.to_owned(), toml::Value::Integer(VERSION));
            return Ok(logins);
        };

        // 1. Text, and TOML.
        let malformed = |why: String| refused(path, "auth_file", &why).with_hint(START_OVER_HINT);
        let text = String::from_utf8(bytes).map_err(|_| malformed("is not UTF-8".to_owned()))?;
        logins.table = text.parse::<toml::Table>().map_err(|e| {
            // The line alone: the parser's own message can quote what the line holds.
            let line = e.span().map_or(1, |span| {
                let before = text.get(..span.start).unwrap_or_default();
                before.matches('\n').count() + 1
            });
            malformed(format!("is not TOML (line {line})"))
        })?;

        // 2. The version, before the shape this version gives the rest.
        match logins.table.get(VERSION_KEY) {
            Some(toml::Value::Integer(VERSION)) => {}
            found => {
                let why = match found {
                    Some(toml::Value::Integer(other)) => {
                        format!("has version {other}, where this build reads version {VERSION}")
                    }
                    _ => format!("names no version, where this build reads version {VERSION}"),
                };
                let hint = "use the SwarmOps that wrote the file, or remove it to start over";
                return Err(refused(path, "auth_file_version", &why).with_hint(hint));
            }
        }

        // 3. The logins, each host once.
        let entries = match logins.table.get(LOGINS_KEY) {
            None => &[][..],
            Some(toml::Value::Array(entries)) => entries.as_slice(),
            Some(_) => return Err(malformed(format!("holds a {LOGINS_KEY} that is no list"))),
        };
        for (at, entry) in entries.iter().enumerate() {
            let listed = entry
                .as_table()
                .ok_or("is no table")
                .and_then(listed)
                .map_err(|why| malformed(format!("holds a login that {why} (entry {})", at + 1)))?;
            if logins.listed.iter().any(|seen| seen.host == listed.host) {
                // The host is a name the file's owner wrote, and no secret.
                let why = format!("holds two logins for {:?}", listed.host);
                return Err(malformed(why));
            }
            logins.listed.push(listed);
        }
        Ok(logins)
    }

    /// Where the file's login with `provider` is, among its entries: the one for the provider's
    /// host. `E_INVALID_ARG` (`details.reason` `provider_mismatch`) when that host's login is
    /// another provider's, which this build neither replaces nor removes.
    fn position(&self, provider: Provider) -> Result<Option<usize>, OpError> {
        let host = provider.host();
        let Some(at) = self.listed.iter().position(|entry| entry.host == host) else {
            return Ok(None);
        };
        let other = &self.listed[at].provider;
        if other == provider.key() {
            return Ok(Some(at));
        }
        let why = format!(
            "holds a login for {host} with provider {other:?}, not {:?}",
            provider.key()
        );
        let mut refused = refused(&self.path, "provider_mismatch", &why);
        refused
            .details
            .insert("provider".to_owned(), Value::from(other.as_str()));
        refused.details.insert("host".to_owned(), Value::from(host));
        Err(refused)
    }

    /// The file's entries, to change: an empty list is made when the file has none.
    fn entries_mut(&mut self) -> &mut Vec<toml::Value> {
        let entries = self
            .table
            .entry(LOGINS_KEY)
            .or_insert_with(|| toml::Value::Array(Vec::new()));

        // `read` let nothing but a list through.
        if !entries.is_array() {
            *entries = toml::Value::Array(Vec::new());
        }
        match entries {
            toml::Value::Array(entries) => entries,
            _ => unreachable!("made a list above"),
        }
    }

    /// Writes the file whole, as the module's "Reading and writing" says: every key and entry
    /// as it stands in `table`, an empty list of logins left out. `E_IO`, naming the file
    /// (`details.auth_file`), when it cannot be written.
    fn write(&mut self) -> Result<(), OpError> {
        let path = &self.path;
        let failed = |why: String| {
            error::error(
                ErrorCode::Io,
                format!("the login file {path} cannot be written: {why}"),
            )
            .with_details(error::details([("auth_file", path.as_str().into())]))
        };

        // 1. The text: the header, then the table.
        let empty = self
            .table
            .get(LOGINS_KEY)
            .and_then(toml::Value::as_array)
            .is_some_and(Vec::is_empty);
        if empty {
            self.table.remove(LOGINS_KEY);
        }
        let body = toml::to_string(&self.table).map_err(|e| failed(e.to_string()))?;
        let text = format!("{HEADER}{body}");

        // 2. The directory, the user's alone when it is made here; then the file, replaced whole.
        let fs = RealFs::new();
        if let Some(dir) = path.parent().filter(|dir| !dir.as_str().is_empty()) {
            fs.create_dir_all(dir.as_std_path())
                .map_err(|e| failed(e.to_string()))?;
        }
        fs.atomic_replace(path.as_std_path(), text.as_bytes())
            .map_err(|e| failed(e.to_string()))
    }
}

/// The login `entry` holds, without its token; else what it lacks, to follow "a login that".
/// The words name the entry's keys, never their values.
fn listed(entry: &toml::Table) -> Result<AuthEntry, &'static str> {
    let text = |key: &str| entry.get(key).and_then(toml::Value::as_str);

    // 1. What every entry has: who it is with, and the host it is looked up by.
    let provider = text(PROVIDER_KEY).ok_or("names no provider")?;
    let host = text(HOST_KEY).ok_or("names no host")?;
    let mut listed = AuthEntry::new(provider, host);
    listed.source = text(SOURCE_KEY).map(str::to_owned);
    listed.scopes = entry
        .get(SCOPES_KEY)
        .and_then(toml::Value::as_array)
        .and_then(|scopes| {
            let scopes = scopes.iter().map(|scope| scope.as_str().map(str::to_owned));
            scopes.collect::<Option<Vec<String>>>()
        });

    // 2. What an entry of a provider this build knows has, since its token may be used: a host
    //    that is a key, a token, and a source and scopes of the shapes this build writes.
    if Provider::named(provider).is_none() {
        return Ok(listed);
    }
    let keyed = [true, false]
        .into_iter()
        .any(|https| host_key(host, https).as_deref() == Some(host));
    if !keyed {
        return Err("names a host that is no host or host:port in lower case");
    }
    if text(TOKEN_KEY).is_none_or(|token| check_token(token).is_err()) {
        return Err("holds no token of 1 to 1024 visible ASCII characters");
    }
    if listed.source.is_none() {
        return Err("names no source");
    }
    if entry.contains_key(SCOPES_KEY) && listed.scopes.is_none() {
        return Err("holds scopes that are no list of strings");
    }
    Ok(listed)
}

/// The bytes of the login file at `path`, which must be the user's alone; `None` when there is
/// no file. The kind, the mode and the size checked are those of the open file the bytes are
/// read from. The refusals are [the module's table](self)'s.
fn read_private(path: &Utf8Path) -> Result<Option<Vec<u8>>, OpError> {
    let unreadable = |e: io::Error| {
        error::error(
            ErrorCode::Io,
            format!("the login file {path} cannot be read: {e}"),
        )
        .with_details(error::details([("auth_file", path.as_str().into())]))
    };
    let other_kind = || {
        refused(
            path,
            "auth_file_kind",
            "is a symbolic link or no regular file",
        )
        .with_hint("keep the login file as a regular file of your own, or remove it")
    };

    // 1. What the path names, without following it: a link is refused, and so is anything an
    //    open could wait on (a FIFO).
    let named = match fs::symlink_metadata(path) {
        Ok(named) => named,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e)),
    };
    if !named.file_type().is_file() {
        return Err(other_kind());
    }

    // 2. The file, opened once; what is checked from here on is the open file's.
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e)),
    };
    let opened = file.metadata().map_err(unreadable)?;
    if !opened.file_type().is_file() {
        return Err(other_kind());
    }
    if opened.mode() & LOOSE_MODE != 0 {
        let why = format!(
            "has mode {:04o}: its group or others can read or write it",
            opened.mode() & 0o7777
        );
        let fix = format!("chmod 600 {}", error::shell_word(path.as_str()));
        return Err(refused(path, "auth_file_mode", &why).with_hint(error::hint(fix)));
    }

    // 3. Its bytes, no further than one past the limit.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        let why = format!("holds more than {MAX_FILE_BYTES} bytes");
        return Err(refused(path, "auth_file", &why).with_hint(START_OVER_HINT));
    }
    Ok(Some(bytes))
}

/// `E_INVALID_ARG` for the login file at `path`, which `why` (it follows "the login file" and
/// the path): `details.reason` is `reason` and `details.auth_file` the path.
fn refused(path: &Utf8Path, reason: &str, why: &str) -> OpError {
    error::error(
        ErrorCode::InvalidArg,
        format!("the login file {path} {why}"),
    )
    .with_details(error::details([
        ("reason", reason.into()),
        ("auth_file", path.as_str().into()),
    ]))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use tempfile::TempDir;

    use super::*;

    /// A token no provider issued, in no provider's shape.
    const TOKEN: &str = "example-token-not-a-secret";

    /// What a file with no login lists.
    const NO_LOGINS: [AuthEntry; 0] = [];

    /// A second one, to tell a replaced login from the first.
    const NEWER: &str = "newer-token-not-a-secret";

    /// A temp directory and the path of a login file in a directory of it that does not exist.
    fn nowhere() -> (TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("a UTF-8 temp dir");
        let path = root.join("state").join("swarmops").join(FILE_NAME);
        (dir, path)
    }

    /// A temp directory and a login file in it holding `text`, with mode 0600.
    fn holding(text: &str) -> (TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("a UTF-8 temp dir");
        let path = root.join(FILE_NAME);
        fs::write(&path, text).expect("write the file");
        chmod(&path, 0o600);
        (dir, path)
    }

    fn chmod(path: &Utf8Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set the mode");
    }

    fn mode(path: &Utf8Path) -> u32 {
        fs::metadata(path).expect("stat").mode() & 0o7777
    }

    /// Logs in with `token` read from stdin, as `auth login --gh --with-token` does.
    fn log_in(path: &Utf8Path, token: &str) -> Result<AuthLogin, OpError> {
        login(Some(path), Provider::Gh, Source::WithToken, None, token)
    }

    /// The error's code and `details.reason`.
    fn reason(error: &OpError) -> (&str, &str) {
        let reason = error.details.get("reason").and_then(Value::as_str);
        (error.code.as_str(), reason.unwrap_or_default())
    }

    /// An environment holding `vars` only.
    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| {
            vars.iter()
                .find(|(n, _)| n == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn a_login_round_trips() {
        let (_dir, path) = nowhere();

        // 1. No file yet: no logins, and a logout that removes nothing writes none.
        let none = status(Some(&path)).unwrap();
        assert_eq!(none.file.as_deref(), Some(path.as_str()));
        assert_eq!(none.logins, NO_LOGINS);
        let nothing = logout(Some(&path), Provider::Gh).unwrap();
        assert!(!nothing.removed);
        assert!(!path.exists(), "a logout of no login makes no file");

        // 2. A login makes the directory and the file, the user's alone.
        let first = log_in(&path, TOKEN).unwrap();
        assert_eq!(first.file, path.as_str());
        assert!(!first.replaced);
        assert_eq!(first.login.provider, "gh");
        assert_eq!(first.login.host, "github.com");
        assert_eq!(first.login.source.as_deref(), Some("with_token"));
        assert_eq!(first.login.scopes, None);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        assert!(text.contains("version = 1"), "{text}");
        assert!(text.contains(&format!("token = \"{TOKEN}\"")), "{text}");

        // 3. Status lists it, without the token.
        let listed = status(Some(&path)).unwrap();
        assert_eq!(listed.logins, std::slice::from_ref(&first.login));
        assert!(!format!("{listed:?} {first:?}").contains(TOKEN));

        // 4. A second login replaces it, with what it was granted.
        let scopes = ["repo".to_owned()];
        let second = login(
            Some(&path),
            Provider::Gh,
            Source::Device,
            Some(&scopes),
            NEWER,
        )
        .unwrap();
        assert!(second.replaced);
        assert_eq!(second.login.source.as_deref(), Some("device"));
        assert_eq!(second.login.scopes.as_deref(), Some(&scopes[..]));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(NEWER) && !text.contains(TOKEN), "{text}");
        assert_eq!(status(Some(&path)).unwrap().logins, [second.login]);
        assert_eq!(mode(&path), 0o600);

        // 5. A logout removes it and leaves a file with no login; a second removes nothing.
        let out = logout(Some(&path), Provider::Gh).unwrap();
        assert!(out.removed);
        assert_eq!(out.file.as_deref(), Some(path.as_str()));
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("{HEADER}version = 1\n"));
        assert_eq!(status(Some(&path)).unwrap().logins, NO_LOGINS);
        assert!(!logout(Some(&path), Provider::Gh).unwrap().removed);
    }

    #[test]
    fn unknown_keys_and_providers_survive_a_rewrite() {
        let (_dir, path) = holding(
            "version = 1\n\
             later = \"kept\"\n\
             \n\
             [[login]]\n\
             provider = \"so\"\n\
             host = \"cloud.example\"\n\
             token = \"cloud-token-not-a-secret\"\n\
             region = \"eu\"\n\
             \n\
             [[login]]\n\
             provider = \"gh\"\n\
             host = \"127.0.0.1:8080\"\n\
             token = \"loopback-token-not-a-secret\"\n\
             source = \"with_token\"\n\
             note = \"kept too\"\n\
             \n\
             [settings]\n\
             color = true\n",
        );

        // 1. Status lists the other provider's login too, as far as it says.
        let before = status(Some(&path)).unwrap().logins;
        assert_eq!(before.len(), 2);
        assert_eq!(before[0], AuthEntry::new("so", "cloud.example"));
        assert_eq!(before[1].host, "127.0.0.1:8080");

        // 2. A login and a logout rewrite the file; all this build does not know is kept.
        log_in(&path, TOKEN).unwrap();
        let with = fs::read_to_string(&path).unwrap().parse::<toml::Table>();
        assert_eq!(with.unwrap()[LOGINS_KEY].as_array().unwrap().len(), 3);
        assert!(logout(Some(&path), Provider::Gh).unwrap().removed);
        let after: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(after["later"].as_str(), Some("kept"));
        assert_eq!(after["settings"]["color"].as_bool(), Some(true));
        let entries = after[LOGINS_KEY].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["provider"].as_str(), Some("so"));
        assert_eq!(entries[0]["region"].as_str(), Some("eu"));
        assert_eq!(
            entries[0]["token"].as_str(),
            Some("cloud-token-not-a-secret")
        );
        assert_eq!(entries[1]["note"].as_str(), Some("kept too"));
        assert_eq!(status(Some(&path)).unwrap().logins, before);
    }

    #[test]
    fn a_file_this_build_does_not_read_is_refused_and_left_alone() {
        let gh = |more: &str| {
            format!(
                "version = 1\n[[login]]\nprovider = \"gh\"\nhost = \"github.com\"\n\
                 token = \"{TOKEN}\"\nsource = \"with_token\"\n{more}"
            )
        };
        let twice = gh(&format!(
            "[[login]]\nprovider = \"so\"\nhost = \"github.com\"\ntoken = \"{TOKEN}\"\n"
        ));
        let cases: &[(&str, String, &str)] = &[
            (
                "another version",
                gh("").replace("= 1", "= 2"),
                "auth_file_version",
            ),
            (
                "no version",
                gh("").replace("version = 1\n", ""),
                "auth_file_version",
            ),
            ("an empty file", String::new(), "auth_file_version"),
            (
                "not TOML",
                format!("version = 1\ntoken = {TOKEN}\n"),
                "auth_file",
            ),
            (
                "not UTF-8",
                "version = 1\n# \u{fffd}".to_owned(),
                "auth_file",
            ),
            (
                "logins that are no list",
                "version = 1\nlogin = 7\n".to_owned(),
                "auth_file",
            ),
            ("a host twice", twice, "auth_file"),
            (
                "no host",
                gh("").replace("host = \"github.com\"\n", ""),
                "auth_file",
            ),
            (
                "no provider",
                gh("").replace("provider = \"gh\"\n", ""),
                "auth_file",
            ),
            (
                "no token",
                gh("").replace(&format!("token = \"{TOKEN}\"\n"), ""),
                "auth_file",
            ),
            ("an empty token", gh("").replace(TOKEN, ""), "auth_file"),
            (
                "no source",
                gh("").replace("source = \"with_token\"\n", ""),
                "auth_file",
            ),
            (
                "a host in upper case",
                gh("").replace("github.com", "GitHub.com"),
                "auth_file",
            ),
            (
                "a host with a path",
                gh("").replace("github.com", "github.com/x"),
                "auth_file",
            ),
            (
                "scopes that are no list",
                gh("scopes = \"repo\"\n"),
                "auth_file",
            ),
            ("too large", gh(&"#\n".repeat(40_000)), "auth_file"),
        ];
        for (what, text, expected) in cases {
            let (_dir, path) = holding(text);
            if *what == "not UTF-8" {
                fs::write(&path, b"version = 1\n# \xff\n").unwrap();
            }
            let before = fs::read(&path).unwrap();

            let calls = [
                status(Some(&path)).map(|_| ()),
                log_in(&path, NEWER).map(|_| ()),
                logout(Some(&path), Provider::Gh).map(|_| ()),
            ];
            for call in calls {
                let error = call.expect_err(what);
                assert_eq!(reason(&error), ("E_INVALID_ARG", *expected), "{what}");
                assert_eq!(error.details["auth_file"], path.as_str(), "{what}");
                let shown = format!("{error:?}");
                assert!(!shown.contains(TOKEN), "{what}: {shown}");
                assert!(error.hint.is_some(), "{what}: a way out");
            }
            assert_eq!(fs::read(&path).unwrap(), before, "{what}: left as it was");
        }
    }

    #[test]
    fn a_loose_file_is_refused_with_the_chmod_that_fixes_it() {
        for loose in [0o644, 0o640, 0o604, 0o620, 0o602, 0o660] {
            let (dir, path) = holding("version = 1\n");
            // A directory name a shell would split.
            let spaced = Utf8Path::from_path(dir.path()).unwrap().join("my state");
            fs::create_dir(&spaced).unwrap();
            let file = spaced.join(FILE_NAME);
            fs::rename(&path, &file).unwrap();
            chmod(&file, loose);

            let error = status(Some(&file)).unwrap_err();
            assert_eq!(reason(&error), ("E_INVALID_ARG", "auth_file_mode"));
            assert_eq!(
                error.hint.as_deref(),
                Some(format!("chmod 600 '{file}'").as_str())
            );
            let error = log_in(&file, TOKEN).unwrap_err();
            assert_eq!(reason(&error), ("E_INVALID_ARG", "auth_file_mode"));
            assert_eq!(mode(&file), loose, "the mode is the user's to change");
            assert_eq!(fs::read_to_string(&file).unwrap(), "version = 1\n");
        }

        // The owner's own bits do not matter: a read-only file is read.
        let (_dir, path) = holding("version = 1\n");
        chmod(&path, 0o400);
        assert_eq!(status(Some(&path)).unwrap().logins, NO_LOGINS);
    }

    #[test]
    fn the_mode_checked_is_the_mode_of_the_bytes_read() {
        // 1. A link to a private file is refused: the path would name whatever it points at.
        let (dir, target) = holding(&format!(
            "version = 1\n[[login]]\nprovider = \"gh\"\nhost = \"github.com\"\n\
             token = \"{TOKEN}\"\nsource = \"with_token\"\n"
        ));
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let link = root.join("link.toml");
        symlink(&target, &link).unwrap();
        for call in [
            status(Some(&link)).map(|_| ()),
            log_in(&link, NEWER).map(|_| ()),
            logout(Some(&link), Provider::Gh).map(|_| ()),
        ] {
            let error = call.unwrap_err();
            assert_eq!(reason(&error), ("E_INVALID_ARG", "auth_file_kind"));
            assert_eq!(error.details["auth_file"], link.as_str());
        }
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(fs::read_to_string(&target).unwrap().contains(TOKEN));

        // 2. A directory is no file, and is not opened.
        let dir_path = root.join("a-directory");
        fs::create_dir(&dir_path).unwrap();
        let error = status(Some(&dir_path)).unwrap_err();
        assert_eq!(reason(&error), ("E_INVALID_ARG", "auth_file_kind"));

        // 3. The checks are the open file's: its mode and size, read from its handle.
        let (_dir, path) = holding("version = 1\n");
        assert_eq!(
            read_private(&path).unwrap().as_deref(),
            Some(&b"version = 1\n"[..])
        );
        chmod(&path, 0o644);
        let error = read_private(&path).unwrap_err();
        assert_eq!(reason(&error), ("E_INVALID_ARG", "auth_file_mode"));
        let (_dir, gone) = nowhere();
        assert_eq!(read_private(&gone).unwrap(), None);
    }

    #[test]
    fn a_login_onto_another_provider_s_host_is_refused() {
        let text = "version = 1\n[[login]]\nprovider = \"so\"\nhost = \"github.com\"\n\
                    token = \"cloud-token-not-a-secret\"\n";
        let (_dir, path) = holding(text);

        let error = log_in(&path, TOKEN).unwrap_err();
        assert_eq!(reason(&error), ("E_INVALID_ARG", "provider_mismatch"));
        assert_eq!(error.details["provider"], "so");
        assert_eq!(error.details["host"], "github.com");
        assert_eq!(error.details["auth_file"], path.as_str());
        let error = logout(Some(&path), Provider::Gh).unwrap_err();
        assert_eq!(reason(&error), ("E_INVALID_ARG", "provider_mismatch"));
        assert_eq!(fs::read_to_string(&path).unwrap(), text, "left as it was");
    }

    #[test]
    fn logout_removes_only_its_entry() {
        let (_dir, path) = holding(
            "version = 1\n[[login]]\nprovider = \"gh\"\nhost = \"127.0.0.1:8080\"\n\
             token = \"loopback-token-not-a-secret\"\nsource = \"with_token\"\n",
        );
        log_in(&path, TOKEN).unwrap();
        assert_eq!(status(Some(&path)).unwrap().logins.len(), 2);

        let out = logout(Some(&path), Provider::Gh).unwrap();
        assert_eq!(
            (out.provider.as_str(), out.host.as_str()),
            ("gh", "github.com")
        );
        assert!(out.removed);
        let left = status(Some(&path)).unwrap().logins;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].host, "127.0.0.1:8080");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("loopback-token-not-a-secret") && !text.contains(TOKEN));
    }

    #[test]
    fn a_token_is_visible_ascii_and_never_quoted() {
        let (_dir, path) = nowhere();
        let long = "x".repeat(MAX_TOKEN_BYTES + 1);
        for bad in [
            "",
            "two words",
            "line\nbreak",
            "tab\there",
            "caf\u{e9}",
            &long,
        ] {
            let error = log_in(&path, bad).unwrap_err();
            assert_eq!(reason(&error), ("E_INVALID_ARG", "token"), "{bad:?}");
            if !bad.is_empty() {
                assert!(!format!("{error:?}").contains(bad), "{bad:?} is not quoted");
            }
        }
        assert!(!path.exists(), "a refused token writes nothing");
        assert!(log_in(&path, &"x".repeat(MAX_TOKEN_BYTES)).is_ok());
    }

    #[test]
    fn the_file_is_where_the_host_keeps_state() {
        // 1. The variable names it, by an absolute path; empty counts as unset.
        let named = env_of(&[
            (AUTH_FILE_ENV, "/somewhere/logins.toml"),
            ("HOME", "/home/me"),
        ]);
        assert_eq!(
            file(&named).unwrap().as_deref(),
            Some(Utf8Path::new("/somewhere/logins.toml"))
        );
        let relative = env_of(&[(AUTH_FILE_ENV, "logins.toml"), ("HOME", "/home/me")]);
        let error = file(&relative).unwrap_err();
        assert_eq!(error.code, "E_INVALID_ARG");
        assert_eq!(error.details["source"], AUTH_FILE_ENV);

        // 2. Else the host's state directory: the same one the memo keys are under.
        let home = env_of(&[(AUTH_FILE_ENV, ""), ("HOME", "/home/me")]);
        let state = userdir::state_dir(&home).unwrap().expect("a state dir");
        assert_eq!(
            file(&home).unwrap(),
            Some(state.join("swarmops").join("auth.toml"))
        );

        // 3. A host that names neither has no file: nothing to list or remove, no login.
        let bare = env_of(&[]);
        assert_eq!(file(&bare).unwrap(), None);
        let none = status(None).unwrap();
        assert_eq!((none.file, none.logins.len()), (None, 0));
        let out = logout(None, Provider::Gh).unwrap();
        assert_eq!((out.file, out.removed), (None, false));
        let error = login(None, Provider::Gh, Source::WithToken, None, TOKEN).unwrap_err();
        assert_eq!(reason(&error), ("E_INVALID_ARG", "auth_dir"));
    }
}
