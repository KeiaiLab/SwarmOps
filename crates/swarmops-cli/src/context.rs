//! What a command runs with besides its arguments: the store the user named, the caller's
//! identity and the output format, each from a flag or else from the environment.
//!
//! - The store is `--repo <store>`, else `SWARMOPS_REPO` (umbrella spec §4), else the `repo` of
//!   the nearest `.swarmws/workspace.json` at or above the working directory (M5, [`discovered`]).
//!   None is `E_NOT_A_REPO`, exit 6.
//! - The identity is `--as <family>/<instance>`, else `SWARMOPS_AGENT`, with `SWARMOPS_RUN_ID`,
//!   as `swarmops_ops::identity` resolves it.
//! - The format is `--json` or `--human`, whichever comes last, else `SWARMOPS_FORMAT`
//!   (`json` or `human`), else JSON; JSON is pretty with `--pretty`, and by default when
//!   stdout is a terminal.
//!
//! - The memo key of a store, which the git export and import trust export oid memos under, is
//!   the file `<repo_id>.key` of `SWARMOPS_MEMO_KEY_DIR`, else of the per-OS state directory's
//!   `swarmops/memo-keys/`, made on first use ([`memo_key`]); no such directory, no key.
//!
//! A variable set to the empty string counts as unset. An explicit empty `--repo` names no
//! store and does not fall back to `SWARMOPS_REPO` (an unset shell variable in `--repo "$X"`
//! must not pick another store). A variable that is not UTF-8 is `E_INVALID_ARG`, said of the
//! variable (`details.source`).
//! Every function reads the environment through `env`, so that tests can hand it one.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_ops::{MEMO_KEY_LEN, MemoKey};
use swarmops_ws::RealFs;
use swarmops_ws::hook::Bootstrap;
use swarmops_ws::workspace::{self, Locate};

use crate::cli::Global;
use crate::output::{Format, Layout, Stdout};

/// The environment variable that names the store when `--repo` is not given.
pub const REPO_ENV: &str = "SWARMOPS_REPO";

/// The environment variable that picks the format when neither `--json` nor `--human` is given.
pub const FORMAT_ENV: &str = "SWARMOPS_FORMAT";

/// The environment variable that moves the directory the hooks make workspaces in.
pub const WS_ROOT_ENV: &str = "SWARMOPS_WS_ROOT";

/// The environment variable that, set to [`BOOTSTRAP_SKIP`], skips a new workspace's bootstrap.
pub const WS_BOOTSTRAP_ENV: &str = "SWARMOPS_WS_BOOTSTRAP";

/// What [`WS_BOOTSTRAP_ENV`] is set to in order to skip the bootstrap.
pub const BOOTSTRAP_SKIP: &str = "skip";

/// The environment variable that names the directory of the stores' memo keys ([`memo_key`]).
pub const MEMO_KEY_DIR_ENV: &str = "SWARMOPS_MEMO_KEY_DIR";

/// The mode of the memo keys' directory, which [`memo_key`] makes: its owner's alone.
#[cfg(unix)]
const MEMO_KEY_DIR_MODE: u32 = 0o700;

/// The mode of a memo key's file: its owner reads and writes it, no one else.
#[cfg(unix)]
const MEMO_KEY_FILE_MODE: u32 = 0o600;

/// How a command reads environment variable `name`.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The store as the user named it: `flag` (`--repo`), else `SWARMOPS_REPO`; `None` when
/// neither names one, and for an empty `--repo`, which is read as naming none.
pub fn store_named(flag: Option<&str>, env: Env<'_>) -> Result<Option<String>, OpError> {
    match flag {
        Some("") => Ok(None),
        Some(store) => Ok(Some(store.to_owned())),
        None => var(env, REPO_ENV),
    }
}

/// The store a command runs on: [`store_named`], else the one a workspace names ([`discovered`])
/// from `start`, the directory the lookup starts from. An explicit empty `--repo` names none and
/// looks nowhere, as it does not fall back to `SWARMOPS_REPO`.
pub fn store_for(
    flag: Option<&str>,
    env: Env<'_>,
    start: Option<&Locate>,
) -> Result<Option<String>, OpError> {
    if let Some(named) = store_named(flag, env)? {
        return Ok(Some(named));
    }
    if flag == Some("") {
        return Ok(None);
    }
    match start {
        Some(at) => discovered(at),
        None => Ok(None),
    }
}

/// The store the workspace `at` names, if there is one: its `workspace.json`'s `repo`. None for
/// a place with no workspace; a `workspace.json` that cannot be read is its error.
pub fn discovered(at: &Locate) -> Result<Option<String>, OpError> {
    match workspace::find(at, None, &RealFs::new()) {
        Ok(found) => Ok(Some(found.file.repo)),
        Err(e) if e.code == ErrorCode::WsNotFound.as_str() => Ok(None),
        Err(e) => Err(e),
    }
}

/// The working directory as a workspace lookup starts from it: none when it cannot be read or is
/// not UTF-8, which no workspace's path is.
pub fn here() -> Option<Utf8PathBuf> {
    let dir = std::env::current_dir().ok()?;
    Utf8PathBuf::from_path_buf(dir).ok()
}

/// Whether a new workspace's bootstrap runs: [`Bootstrap::Skip`] when [`WS_BOOTSTRAP_ENV`] is
/// [`BOOTSTRAP_SKIP`].
pub fn bootstrap(env: Env<'_>) -> Result<Bootstrap, OpError> {
    match var(env, WS_BOOTSTRAP_ENV)?.as_deref() {
        Some(BOOTSTRAP_SKIP) => Ok(Bootstrap::Skip),
        _ => Ok(Bootstrap::Run),
    }
}

/// The directory the hooks make workspaces in: `SWARMOPS_WS_ROOT`, else the per-OS state
/// directory (`$XDG_STATE_HOME/swarmops/workspaces`, `~/.local/state/…` by default, on Linux),
/// else `E_INVALID_ARG` for a host that names none.
pub fn ws_root(env: Env<'_>) -> Result<Utf8PathBuf, OpError> {
    if let Some(root) = var(env, WS_ROOT_ENV)? {
        return Ok(Utf8PathBuf::from(root));
    }
    let state = state_dir(env)?;
    Ok(state.join("swarmops").join("workspaces"))
}

/// The memo key of the store whose id is `repo_id` (operations spec §1, storage spec §5): 32
/// bytes in `<dir>/<repo_id>.key`, `<dir>` being [`MEMO_KEY_DIR_ENV`], else the per-OS state
/// directory's `swarmops/memo-keys` (`$XDG_STATE_HOME/swarmops/memo-keys`, `~/.local/state/…` by
/// default, on Linux). A key the file does not hold yet is made: 32 random bytes in a temp file
/// of mode 0600, linked into place without replacing anything, the directory (made 0700) synced,
/// so that of two makers racing one wins and both read its key. `None` for a host with no state
/// directory, or a store id that is no 32 lowercase hex digits: the export and the import then
/// derive every export oid. A file of another length, or one that cannot be read or made, is
/// `E_INVALID_ARG` naming the file (`details.key_file`), never overwritten.
pub fn memo_key(env: Env<'_>, repo_id: &str) -> Result<Option<MemoKey>, OpError> {
    let dir = match var(env, MEMO_KEY_DIR_ENV)? {
        Some(dir) => Utf8PathBuf::from(dir),
        None => match state_dir(env) {
            Ok(state) => state.join("swarmops").join("memo-keys"),
            Err(_) => return Ok(None),
        },
    };
    let is_id = repo_id.len() == 32
        && repo_id
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !is_id {
        return Ok(None);
    }
    let file = dir.join(format!("{repo_id}.key"));
    let failed = |why: String| {
        let mut details = Map::new();
        details.insert("key_file".to_owned(), Value::from(file.as_str()));
        OpError::new(ErrorCode::InvalidArg, format!("the memo key {file}: {why}"))
            .with_details(details)
    };

    // 1. The key the file holds; made first if there is none.
    let bytes = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            make_key(&dir, &file).map_err(|e| failed(format!("cannot be made: {e}")))?;
            fs::read(&file).map_err(|e| failed(format!("cannot be read: {e}")))?
        }
        Err(e) => return Err(failed(format!("cannot be read: {e}"))),
    };

    // 2. Exactly a key's bytes, or the file is not one.
    let Ok(key) = <[u8; MEMO_KEY_LEN]>::try_from(bytes.as_slice()) else {
        let why = format!(
            "holds {} bytes, where a key has {MEMO_KEY_LEN}",
            bytes.len()
        );
        return Err(failed(why));
    };
    Ok(Some(MemoKey::from_bytes(key)))
}

/// Makes the key file `file` in `dir`, unless one is there: 32 random bytes written to a temp file
/// of its own in `dir`, synced, and hard-linked to `file`, which replaces nothing (a maker that
/// lost the race leaves the winner's); then the temp file removed and `dir` synced.
fn make_key(dir: &Utf8Path, file: &Utf8Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(MEMO_KEY_DIR_MODE);
    builder.create(dir)?;

    // 1. The key, durable in a temp file no other maker names.
    let mut key = [0; MEMO_KEY_LEN];
    getrandom::fill(&mut key).map_err(|e| io::Error::other(e.to_string()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let temp = dir.join(format!(".{}.{nanos}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(MEMO_KEY_FILE_MODE);
    let written = options.open(&temp).and_then(|mut out| {
        out.write_all(&key)?;
        out.sync_all()
    });

    // 2. Linked into place without replacing a winner's, then the directory synced.
    let linked = written.and_then(|()| match fs::hard_link(&temp, file) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    });
    let _ = fs::remove_file(&temp);
    linked?;
    File::open(dir)?.sync_all()
}

/// The per-OS directory for state that is neither config nor cache.
fn state_dir(env: Env<'_>) -> Result<Utf8PathBuf, OpError> {
    let home = || {
        var(env, "HOME")?.map(Utf8PathBuf::from).ok_or_else(|| {
            OpError::new(
                ErrorCode::InvalidArg,
                format!("no directory for the workspaces: set {WS_ROOT_ENV}"),
            )
        })
    };
    if cfg!(windows) {
        return var(env, "LOCALAPPDATA")?
            .map(Utf8PathBuf::from)
            .ok_or_else(|| {
                OpError::new(
                    ErrorCode::InvalidArg,
                    format!("no directory for the workspaces: set {WS_ROOT_ENV}"),
                )
            });
    }
    if cfg!(target_os = "macos") {
        return Ok(home()?.join("Library").join("Application Support"));
    }
    match var(env, "XDG_STATE_HOME")? {
        Some(state) if Utf8Path::new(&state).is_absolute() => Ok(Utf8PathBuf::from(state)),
        _ => Ok(home()?.join(".local").join("state")),
    }
}

/// `E_NOT_A_REPO` for a command that needs a store when the user named none, given `flag`
/// (`--repo`): the message says when it was given empty, which names none.
pub fn no_store_named(flag: Option<&str>) -> OpError {
    let why = match flag {
        Some(_) => "--repo is empty".to_owned(),
        None => format!("neither --repo nor {REPO_ENV} gives one"),
    };
    OpError::new(ErrorCode::NotARepo, format!("no store named: {why}")).with_hint(format!(
        "pass --repo <store> or set {REPO_ENV}; swarmops init <dir> creates a store"
    ))
}

/// The caller's identity, if any: `flag` (`--as`), else the environment.
pub fn identity(flag: Option<&str>, env: Env<'_>) -> Result<Option<Identity>, OpError> {
    swarmops_ops::identity::resolve_identity_from(flag, env)
}

/// The identity a mutation acts as: [`identity`], or `E_IDENTITY_REQUIRED` (exit 5).
pub fn mutator(flag: Option<&str>, env: Env<'_>) -> Result<Identity, OpError> {
    swarmops_ops::identity::require_identity(identity(flag, env)?)
}

/// The format `global`'s flags and the environment ask for, given what `stdout` is.
/// `E_INVALID_ARG` for a `SWARMOPS_FORMAT` that is neither `json` nor `human`; the error is
/// then printed as [`json_format`] gives.
pub fn format(global: &Global, env: Env<'_>, stdout: Stdout) -> Result<Format, OpError> {
    let human = if global.human || global.json {
        global.human
    } else {
        match var(env, FORMAT_ENV)?.as_deref() {
            None | Some("json") => false,
            Some("human") => true,
            Some(other) => {
                return Err(said_of(
                    OpError::new(
                        ErrorCode::InvalidArg,
                        format!("{other:?} is not a format: json or human"),
                    )
                    .with_hint(format!("set {FORMAT_ENV} to json or human, or unset it")),
                    FORMAT_ENV,
                ));
            }
        }
    };
    Ok(if human {
        Format::Human
    } else {
        json_format(global, stdout)
    })
}

/// JSON, pretty as `--pretty` says, else when stdout is a terminal.
pub fn json_format(global: &Global, stdout: Stdout) -> Format {
    let layout = match global.pretty {
        Some(true) => Layout::Pretty,
        Some(false) => Layout::Compact,
        None => stdout.layout(),
    };
    Format::Json { layout }
}

/// Environment variable `name` as `env` gives it: `None` when it is unset or empty,
/// `E_INVALID_ARG` when it is not UTF-8.
fn var(env: Env<'_>, name: &str) -> Result<Option<String>, OpError> {
    match env(name) {
        Some(value) if !value.is_empty() => value.into_string().map(Some).map_err(|_| {
            said_of(
                OpError::new(ErrorCode::InvalidArg, "the value is not UTF-8"),
                name,
            )
        }),
        _ => Ok(None),
    }
}

/// `error`, said of `source`: its message prefixed with the variable's name, which
/// `details.source` holds too (as `swarmops_ops::identity` does for its variables).
fn said_of(mut error: OpError, source: &str) -> OpError {
    error.message = format!("{source}: {}", error.message);
    let mut details = Map::new();
    details.insert("source".to_owned(), Value::from(source));
    error.with_details(details)
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    /// An environment holding `vars` only.
    fn env_of(vars: &[(&str, &[u8])]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from_vec(value.to_vec())))
            .collect();
        move |name| {
            vars.iter()
                .find(|(n, _)| n == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn the_store_is_the_flag_else_the_variable() {
        let env = env_of(&[(REPO_ENV, b"/from/env")]);
        assert_eq!(
            store_named(Some("/flag"), &env).unwrap().as_deref(),
            Some("/flag")
        );
        assert_eq!(
            store_named(None, &env).unwrap().as_deref(),
            Some("/from/env")
        );
        // An explicit empty flag names no store, and does not fall back to the variable.
        assert_eq!(store_named(Some(""), &env).unwrap(), None);
        // An empty variable counts as unset.
        let empty = env_of(&[(REPO_ENV, b"")]);
        assert_eq!(store_named(None, &empty).unwrap(), None);
        assert_eq!(store_named(None, &env_of(&[])).unwrap(), None);
        let bad = store_named(None, &env_of(&[(REPO_ENV, b"/\xff")])).unwrap_err();
        assert_eq!(bad.code, "E_INVALID_ARG");
        assert_eq!(bad.details["source"], REPO_ENV);
        assert!(
            bad.message.starts_with("SWARMOPS_REPO: "),
            "{}",
            bad.message
        );
        let none = no_store_named(None);
        assert_eq!((none.code.as_str(), none.exit), ("E_NOT_A_REPO", 6));
        let empty = no_store_named(Some(""));
        assert_eq!(empty.message, "no store named: --repo is empty");
        assert_eq!((empty.code, empty.hint), (none.code, none.hint));
    }

    #[test]
    fn a_mutation_needs_an_identity() {
        let anonymous = env_of(&[]);
        assert_eq!(identity(None, &anonymous).unwrap(), None);
        let refused = mutator(None, &anonymous).unwrap_err();
        assert_eq!(
            (refused.code.as_str(), refused.exit),
            ("E_IDENTITY_REQUIRED", 5)
        );
        let env = env_of(&[
            ("SWARMOPS_AGENT", b"human/purple"),
            ("SWARMOPS_RUN_ID", b"r1"),
        ]);
        let who = mutator(None, &env).unwrap();
        assert_eq!((who.agent(), who.run_id()), ("human/purple", Some("r1")));
        assert_eq!(mutator(Some("a/b"), &env).unwrap().agent(), "a/b");
    }

    #[test]
    fn the_format_is_the_last_flag_else_the_variable() {
        let flags = |json, human, pretty| Global {
            json,
            human,
            pretty,
            ..Global::default()
        };
        let none = env_of(&[]);
        let human = env_of(&[(FORMAT_ENV, b"human")]);
        let json = Format::Json {
            layout: Layout::Compact,
        };
        assert_eq!(
            format(&flags(false, false, None), &none, Stdout::Other).unwrap(),
            json
        );
        assert_eq!(
            format(&flags(false, false, None), &none, Stdout::Terminal).unwrap(),
            Format::Json {
                layout: Layout::Pretty
            },
            "pretty on a terminal"
        );
        assert_eq!(
            format(&flags(false, false, Some(false)), &none, Stdout::Terminal).unwrap(),
            json
        );
        assert_eq!(
            format(&flags(false, false, None), &human, Stdout::Other).unwrap(),
            Format::Human
        );
        assert_eq!(
            format(&flags(true, false, None), &human, Stdout::Other).unwrap(),
            json
        );
        assert_eq!(
            format(&flags(false, true, None), &none, Stdout::Other).unwrap(),
            Format::Human
        );
        let explicit = env_of(&[(FORMAT_ENV, b"json")]);
        assert_eq!(
            format(&flags(false, false, None), &explicit, Stdout::Other).unwrap(),
            json
        );
        let yaml = env_of(&[(FORMAT_ENV, b"yaml")]);
        let bad = format(&flags(false, false, None), &yaml, Stdout::Other).unwrap_err();
        assert_eq!((bad.code.as_str(), bad.exit), ("E_INVALID_ARG", 2));
        assert_eq!(bad.details["source"], FORMAT_ENV);
        // A flag means the variable is not read at all.
        assert_eq!(
            format(&flags(true, false, None), &yaml, Stdout::Other).unwrap(),
            json
        );
    }
}
