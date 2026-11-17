//! The credential a call to a git remote goes with: where a login's token is used.
//!
//! Every call that reaches a git remote (`ls-remote` for `remote add` and `clone`, `fetch`,
//! `push`, and `sync`'s two) asks [`credential`] once. The order is:
//!
//! 1. A remote that names a token variable (`--token-env`) goes with that variable alone, set
//!    or not: the login file is not opened.
//! 2. Otherwise a remote over https, or over http to a loopback address, goes with the token
//!    of the login kept for its host, if there is one: the URL's
//!    [`credential_host`](RemoteUrl::credential_host) and the login's `host` are equal, so no
//!    subdomain's login and no other port's is sent. Only a login of a provider this build
//!    knows is used.
//! 3. Otherwise no token is sent, and git's own credentials apply as they always did.
//!
//! An ssh remote has no credential host, so it never opens the file either. A login file that
//! is there and is refused (a loose mode, another version, not TOML) fails the call before git
//! runs: a token is not skipped silently for a file the user must fix.
//!
//! No `GH_TOKEN` or `GITHUB_TOKEN` is read. A native (`so://`) remote does not come here: its
//! link is ssh. A provider of native remotes would look its token up where the link is made
//! (`native::ssh`), by the same host rule.

use std::ffi::OsString;

use swarmops_api::OpError;
use swarmops_git::transport::{Credential, RemoteUrl};

use super::file;

/// The credential a call to the git remote at `url` goes with, as [the module](self) orders it:
/// `token_env` is the variable the remote names, and `env` the environment the login file is
/// found in ([`file::file`]).
///
/// `E_INVALID_ARG` when the login file is needed and its path or the file itself is refused
/// (`details.source`, or `details.reason` `auth_file_kind`, `auth_file_mode`,
/// `auth_file_version` or `auth_file`); `E_IO` when it cannot be read.
pub(crate) fn credential(
    url: &RemoteUrl,
    token_env: Option<&str>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Credential, OpError> {
    let mut cred = Credential::default();
    cred.token_env = token_env.map(str::to_owned);
    if cred.token_env.is_some() {
        return Ok(cred);
    }
    let Some(host) = url.credential_host() else {
        return Ok(cred);
    };
    let Some(file) = file::file(env)? else {
        return Ok(cred);
    };
    cred.stored = file::token_for(&file, &host)?;
    Ok(cred)
}

/// The process's own environment: what the remote operations hand [`credential`]. The one
/// place this module's callers read it, so a test of the lookup hands in its own.
///
/// A test that runs a remote operation inside the test process, not through the binary the
/// harness isolates, therefore looks the login up in the developer's own environment: no token
/// of theirs is sent to a test's loopback port, since hosts match exactly, but a login file of
/// theirs that is refused fails such a test.
pub(crate) fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use camino::Utf8Path;
    use serde_json::Value;
    use swarmops_git::transport::StoredToken;

    use super::*;
    use crate::auth::AUTH_FILE_ENV;

    /// Tokens no provider issued, in no provider's shape.
    const TOKEN: &str = "example-token-not-a-secret";
    const LOOPBACK_TOKEN: &str = "loopback-token-not-a-secret";

    /// A login for github.com, one for a loopback port, and one of a provider no build knows.
    const LOGINS: &str = "version = 1\n\
        [[login]]\nprovider = \"gh\"\nhost = \"github.com\"\n\
        token = \"example-token-not-a-secret\"\nsource = \"with_token\"\n\
        [[login]]\nprovider = \"gh\"\nhost = \"127.0.0.1:8080\"\n\
        token = \"loopback-token-not-a-secret\"\nsource = \"with_token\"\n\
        [[login]]\nprovider = \"so\"\nhost = \"cloud.example\"\n\
        token = \"cloud-token-not-a-secret\"\n";

    fn url(text: &str) -> RemoteUrl {
        RemoteUrl::parse(text).expect("a remote's URL")
    }

    #[test]
    fn only_an_https_remote_without_a_variable_opens_the_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = Utf8Path::from_path(dir.path())
            .expect("UTF-8")
            .join("auth.toml");
        fs::write(&path, LOGINS).expect("write the file");
        let chmod = |mode| {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set the mode");
        };
        let named = path.as_str().to_owned();
        let env = move |name: &str| (name == AUTH_FILE_ENV).then(|| OsString::from(&named));
        let with = |text: &str, token_env: Option<&str>| credential(&url(text), token_env, &env);

        // 1. A file that would be refused is not opened for an ssh remote, in either form, nor
        //    for a remote that names its variable: each goes as it did before there were logins.
        chmod(0o644);
        for ssh in [
            "ssh://git@github.com/acme/repo.git",
            "git@github.com:acme/repo.git",
        ] {
            assert_eq!(with(ssh, None).unwrap(), Credential::default(), "{ssh}");
        }
        let by_variable = with("https://github.com/acme/repo.git", Some("ACME_TOKEN")).unwrap();
        assert_eq!(by_variable.token_env.as_deref(), Some("ACME_TOKEN"));
        assert_eq!(by_variable.stored, None);

        // 2. An https remote without one opens it, and a refused file fails the call.
        let error = with("https://github.com/acme/repo.git", None).unwrap_err();
        assert_eq!(error.code, "E_INVALID_ARG");
        assert_eq!(error.details["reason"], "auth_file_mode");
        assert_eq!(error.details["auth_file"], path.as_str());
        assert!(!format!("{error:?}").contains(TOKEN));

        // 3. The file read: the login of the URL's own host, by its key.
        chmod(0o600);
        for (text, token) in [
            ("https://github.com/acme/repo.git", Some(TOKEN)),
            ("https://GitHub.com:443/acme/repo.git", Some(TOKEN)),
            ("http://127.0.0.1:8080/repo.git", Some(LOOPBACK_TOKEN)),
            ("https://gist.github.com/acme/repo.git", None),
            ("https://github.com:8443/acme/repo.git", None),
            ("https://github.com.evil.example/acme/repo.git", None),
            ("http://127.0.0.1:8081/repo.git", None),
            ("http://127.0.0.1/repo.git", None),
            ("https://127.0.0.1:8080/repo.git", Some(LOOPBACK_TOKEN)),
            // A provider this build does not know: its token is never sent.
            ("https://cloud.example/store", None),
        ] {
            let cred = with(text, None).unwrap();
            let token = token.map(|token| StoredToken::new(token.to_owned()));
            assert_eq!(cred.stored, token, "{text}");
            assert_eq!(cred.token_env, None, "{text}");
            assert!(!format!("{cred:?}").contains("not-a-secret"), "{cred:?}");
        }

        // 4. A variable named wins over a login for the same host, set or not.
        let cred = with("https://github.com/acme/repo.git", Some("UNSET")).unwrap();
        assert_eq!(cred.stored, None);

        // 5. A host with no login file, and one whose file is not there: no token, no error.
        let bare = |_: &str| None;
        let cred = credential(&url("https://github.com/acme/repo.git"), None, &bare).unwrap();
        assert_eq!(cred, Credential::default());
        fs::remove_file(&path).expect("remove the file");
        let cred = with("https://github.com/acme/repo.git", None).unwrap();
        assert_eq!(cred, Credential::default());

        // 6. A path that is not absolute is said of the variable.
        let relative = |name: &str| (name == AUTH_FILE_ENV).then(|| OsString::from("auth.toml"));
        let refused = credential(&url("https://github.com/acme/repo.git"), None, &relative);
        let refused = refused.unwrap_err();
        assert_eq!(
            refused.details.get("source"),
            Some(&Value::from(AUTH_FILE_ENV))
        );
    }
}
