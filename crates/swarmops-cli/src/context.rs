//! What a command runs with besides its arguments: the store the user named, the caller's
//! identity and the output format, each from a flag or else from the environment.
//!
//! - The store is `--repo <store>`, else `SWARMOPS_REPO` (umbrella spec §4; `.swarmws`
//!   discovery arrives with workspaces). Neither is `E_NOT_A_REPO`, exit 6.
//! - The identity is `--as <family>/<instance>`, else `SWARMOPS_AGENT`, with `SWARMOPS_RUN_ID`,
//!   as `swarmops_ops::identity` resolves it.
//! - The format is `--json` or `--human`, whichever comes last, else `SWARMOPS_FORMAT`
//!   (`json` or `human`), else JSON; JSON is pretty with `--pretty`, and by default when
//!   stdout is a terminal.
//!
//! A variable set to the empty string counts as unset, and so does an empty `--repo`; a
//! variable that is not UTF-8 is `E_INVALID_ARG`, said of the variable (`details.source`).
//! Every function reads the environment through `env`, so that tests can hand it one.

use std::ffi::OsString;

use serde_json::{Map, Value};
use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_ops::identity::{require_identity, resolve_identity_from};

use crate::cli::Global;
use crate::output::Format;

/// The environment variable that names the store when `--repo` is not given.
pub const REPO_ENV: &str = "SWARMOPS_REPO";

/// The environment variable that picks the format when neither `--json` nor `--human` is given.
pub const FORMAT_ENV: &str = "SWARMOPS_FORMAT";

/// How a command reads environment variable `name`.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The store as the user named it: `flag` (`--repo`), else `SWARMOPS_REPO`; `None` when
/// neither names one.
pub fn store_named(flag: Option<&str>, env: Env<'_>) -> Result<Option<String>, OpError> {
    match flag {
        Some(store) if !store.is_empty() => Ok(Some(store.to_owned())),
        _ => var(env, REPO_ENV),
    }
}

/// `E_NOT_A_REPO` for a command that needs a store when the user named none.
pub fn no_store_named() -> OpError {
    OpError::new(
        ErrorCode::NotARepo,
        format!("no store named: neither --repo nor {REPO_ENV} gives one"),
    )
    .with_hint(format!(
        "pass --repo <store> or set {REPO_ENV}; swarmops init <dir> creates a store"
    ))
}

/// The caller's identity, if any: `flag` (`--as`), else the environment.
pub fn identity(flag: Option<&str>, env: Env<'_>) -> Result<Option<Identity>, OpError> {
    resolve_identity_from(flag, env)
}

/// The identity a mutation acts as: [`identity`], or `E_IDENTITY_REQUIRED` (exit 5).
pub fn mutator(flag: Option<&str>, env: Env<'_>) -> Result<Identity, OpError> {
    require_identity(identity(flag, env)?)
}

/// The format `global`'s flags and the environment ask for, given whether stdout is a
/// terminal. `E_INVALID_ARG` for a `SWARMOPS_FORMAT` that is neither `json` nor `human`; the
/// error is then printed as [`json_format`] gives.
pub fn format(global: &Global, env: Env<'_>, terminal: bool) -> Result<Format, OpError> {
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
        json_format(global, terminal)
    })
}

/// JSON, pretty as `--pretty` says, else when stdout is a terminal.
pub fn json_format(global: &Global, terminal: bool) -> Format {
    Format::Json {
        pretty: global.pretty.unwrap_or(terminal),
    }
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
        // Empty counts as unset, for the flag and the variable alike.
        assert_eq!(
            store_named(Some(""), &env).unwrap().as_deref(),
            Some("/from/env")
        );
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
        let none = no_store_named();
        assert_eq!((none.code.as_str(), none.exit), ("E_NOT_A_REPO", 6));
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
        let json = Format::Json { pretty: false };
        assert_eq!(
            format(&flags(false, false, None), &none, false).unwrap(),
            json
        );
        assert_eq!(
            format(&flags(false, false, None), &none, true).unwrap(),
            Format::Json { pretty: true },
            "pretty on a terminal"
        );
        assert_eq!(
            format(&flags(false, false, Some(false)), &none, true).unwrap(),
            json
        );
        assert_eq!(
            format(&flags(false, false, None), &human, false).unwrap(),
            Format::Human
        );
        assert_eq!(
            format(&flags(true, false, None), &human, false).unwrap(),
            json
        );
        assert_eq!(
            format(&flags(false, true, None), &none, false).unwrap(),
            Format::Human
        );
        let explicit = env_of(&[(FORMAT_ENV, b"json")]);
        assert_eq!(
            format(&flags(false, false, None), &explicit, false).unwrap(),
            json
        );
        let yaml = env_of(&[(FORMAT_ENV, b"yaml")]);
        let bad = format(&flags(false, false, None), &yaml, false).unwrap_err();
        assert_eq!((bad.code.as_str(), bad.exit), ("E_INVALID_ARG", 2));
        assert_eq!(bad.details["source"], FORMAT_ENV);
        // A flag means the variable is not read at all.
        assert_eq!(
            format(&flags(true, false, None), &yaml, false).unwrap(),
            json
        );
    }
}
