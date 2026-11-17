//! The host's per-user directory for what SwarmOps keeps outside every store: state that is
//! neither configuration nor cache.
//!
//! | Host | Directory |
//! | --- | --- |
//! | Windows | `%LOCALAPPDATA%` |
//! | macOS | `$HOME/Library/Application Support` |
//! | any other | `$XDG_STATE_HOME` when it is an absolute path, else `$HOME/.local/state` |
//!
//! A caller joins `swarmops/<what it keeps>` to it, and decides what a host that names no such
//! directory means for it: [`state_dir`] gives `None` there and words no refusal. The
//! environment is read through `env`, so that tests can hand it one.

use std::ffi::OsString;

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::OpError;

use crate::identity;

/// The kinds of host [`state_dir`] tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    /// Windows: `%LOCALAPPDATA%`.
    Windows,
    /// macOS: `Library/Application Support` under the home directory.
    MacOs,
    /// Any other, Linux among them: the XDG state directory.
    Xdg,
}

impl Host {
    /// The host this build runs on.
    const THIS: Host = if cfg!(windows) {
        Host::Windows
    } else if cfg!(target_os = "macos") {
        Host::MacOs
    } else {
        Host::Xdg
    };
}

/// The per-OS state directory, as `env` names it (the module's table): `None` on a host whose
/// environment names none. A variable set to the empty string counts as unset, and one that is
/// not UTF-8 is `E_INVALID_ARG`, said of the variable (`details.source`).
pub fn state_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Result<Option<Utf8PathBuf>, OpError> {
    state_dir_on(Host::THIS, env)
}

/// [`state_dir`] of a host of kind `host`.
fn state_dir_on(
    host: Host,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Option<Utf8PathBuf>, OpError> {
    let dir = |name: &str| Ok(identity::var(&env, name)?.map(Utf8PathBuf::from));
    match host {
        Host::Windows => dir("LOCALAPPDATA"),
        Host::MacOs => {
            let home: Option<Utf8PathBuf> = dir("HOME")?;
            Ok(home.map(|home| home.join("Library").join("Application Support")))
        }
        Host::Xdg => {
            // A relative XDG_STATE_HOME is ignored, as the XDG base directory rules ask.
            if let Some(state) = dir("XDG_STATE_HOME")?
                && Utf8Path::new(&state).is_absolute()
            {
                return Ok(Some(state));
            }
            let home: Option<Utf8PathBuf> = dir("HOME")?;
            Ok(home.map(|home| home.join(".local").join("state")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use swarmops_api::ErrorCode;

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

    /// [`state_dir_on`] `host` with `vars`, as text.
    fn on(host: Host, vars: &[(&str, &[u8])]) -> Option<String> {
        state_dir_on(host, &env_of(vars))
            .expect("a readable environment")
            .map(Utf8PathBuf::into_string)
    }

    #[test]
    fn each_host_keeps_state_where_its_own_rules_say() {
        let all: &[(&str, &[u8])] = &[
            ("HOME", b"/home/me"),
            ("XDG_STATE_HOME", b"/xdg/state"),
            ("LOCALAPPDATA", b"C:\\Users\\me\\AppData\\Local"),
        ];

        assert_eq!(
            on(Host::Windows, all).as_deref(),
            Some("C:\\Users\\me\\AppData\\Local")
        );
        assert_eq!(
            on(Host::MacOs, all).as_deref(),
            Some("/home/me/Library/Application Support")
        );
        assert_eq!(on(Host::Xdg, all).as_deref(), Some("/xdg/state"));
    }

    #[test]
    fn the_xdg_directory_falls_back_to_the_home_s() {
        let home: (&str, &[u8]) = ("HOME", b"/home/me");

        // Unset, empty or relative: the default under the home directory.
        for state in [None, Some(&b""[..]), Some(&b"relative/state"[..])] {
            let mut vars = vec![home];
            vars.extend(state.map(|state| ("XDG_STATE_HOME", state)));
            assert_eq!(
                on(Host::Xdg, &vars).as_deref(),
                Some("/home/me/.local/state"),
                "{state:?}"
            );
        }

        // An absolute one needs no home directory.
        assert_eq!(
            on(Host::Xdg, &[("XDG_STATE_HOME", b"/xdg/state")]).as_deref(),
            Some("/xdg/state")
        );
    }

    #[test]
    fn a_host_that_names_no_directory_has_none() {
        // 1. Nothing set, or set empty.
        for host in [Host::Windows, Host::MacOs, Host::Xdg] {
            assert_eq!(on(host, &[]), None, "{host:?}");
            let empty: &[(&str, &[u8])] = &[
                ("HOME", b""),
                ("XDG_STATE_HOME", b""),
                ("LOCALAPPDATA", b""),
            ];
            assert_eq!(on(host, empty), None, "{host:?}");
        }

        // 2. Another host's variable is not read.
        assert_eq!(on(Host::Windows, &[("HOME", b"/home/me")]), None);
        assert_eq!(on(Host::MacOs, &[("XDG_STATE_HOME", b"/xdg/state")]), None);
        assert_eq!(on(Host::Xdg, &[("LOCALAPPDATA", b"C:\\Local")]), None);
        assert_eq!(on(Host::Xdg, &[("XDG_STATE_HOME", b"relative")]), None);
    }

    #[test]
    fn a_variable_that_is_not_utf8_is_refused_by_name() {
        let cases = [
            (Host::Windows, "LOCALAPPDATA"),
            (Host::MacOs, "HOME"),
            (Host::Xdg, "XDG_STATE_HOME"),
            (Host::Xdg, "HOME"),
        ];
        for (host, name) in cases {
            let error = state_dir_on(host, &env_of(&[(name, b"/\xff")]))
                .expect_err("a value that is not UTF-8");
            assert_eq!(
                error.code,
                ErrorCode::InvalidArg.as_str(),
                "{host:?} {name}"
            );
            assert_eq!(
                error.message,
                format!("{name}: the value is not UTF-8"),
                "{host:?}"
            );
            assert_eq!(error.details["source"], name, "{host:?}");
        }
    }

    #[test]
    fn this_build_s_host_is_one_of_the_three() {
        let expected = if cfg!(target_os = "macos") {
            "/home/me/Library/Application Support"
        } else if cfg!(windows) {
            "/local"
        } else {
            "/home/me/.local/state"
        };
        let env = env_of(&[("HOME", b"/home/me"), ("LOCALAPPDATA", b"/local")]);
        let dir = state_dir(&env).expect("a readable environment");
        assert_eq!(dir.as_deref().map(Utf8Path::as_str), Some(expected));
    }
}
