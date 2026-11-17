//! git's credential-helper protocol, answered from the login file: what `auth git-credential`
//! does, so that a plain `git` can use a login (`git -c credential.helper='!so auth
//! git-credential' push`).
//!
//! git runs a helper with one word, the operation, and writes the request on its stdin:
//! `key=value` lines, ended by a blank line or the end of input. [`git_credential`] answers
//! `get` alone, and only for a request a login's token may go with:
//!
//! 1. `protocol` is `https`, or `http` with a `host` that is a literal loopback address (the
//!    rule a remote's URL is held to).
//! 2. `host`, as a [`host_key`] under that protocol, equals the `host` of a login whose provider
//!    this build knows: the exact-host rule of the remote commands (`lookup`), so no
//!    subdomain's login and no other port's is given.
//!
//! Anything else has no answer: another protocol, a host with no login, a request without the
//! two keys. git then asks its next helper. Keys this build does not know are passed over, and
//! of a key met twice the last stands, as git reads its own. `store` and `erase`, which git
//! sends after a remote took or refused a credential, do nothing, as does any other word: one
//! refusal must not delete a login, and the helper stores nothing a login did not.
//!
//! The login file is opened only for a `get` that passes rule 1 and names a host with a key. A
//! file that is there and is refused is the file's own error, never an empty answer.
//!
//! The answer is the one thing in SwarmOps that writes a token out. It never says `quit`, so git
//! stays free to ask other helpers.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::net::IpAddr;

use swarmops_api::OpError;
use swarmops_git::transport::host_key;

use super::file;

/// The most bytes of a request [`git_credential`] is given: what its caller reads of stdin at
/// most. git's own requests are a few hundred bytes.
pub const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// The operation that asks for a credential.
const GET: &str = "get";

/// The user name a token goes with: the one the remote commands' header carries
/// (`swarmops_git`'s transport), GitHub's for an installation token; a personal token takes any.
const USER: &str = "x-access-token";

/// What `auth git-credential get` answers git: a user name and the login's token. Its `Debug`
/// shows no part of the token; [`HelperAnswer::write_to`] is the one way out.
pub struct HelperAnswer {
    token: String,
}

impl HelperAnswer {
    /// Writes the answer as git reads it: a `username=` line and a `password=` line, and nothing
    /// else.
    pub fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(out, "username={USER}")?;
        writeln!(out, "password={}", self.token)
    }
}

impl fmt::Debug for HelperAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HelperAnswer(<redacted>)")
    }
}

/// What the helper answers to git's `operation` with the request `request` (what git wrote on
/// stdin, at most [`MAX_REQUEST_BYTES`] of it), as this module (`helper`) rules: the login's
/// token for a `get` whose protocol and host are a login's, `None` for everything else. `env` is
/// the environment the login file is found in ([`file()`](super::file())).
///
/// `E_INVALID_ARG` when the login file is needed and its path or the file itself is refused
/// (`details.source`, or `details.reason` `auth_file_kind`, `auth_file_mode`,
/// `auth_file_version` or `auth_file`); `E_IO` when it cannot be read.
pub fn git_credential(
    operation: &str,
    request: &[u8],
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Option<HelperAnswer>, OpError> {
    if operation != GET {
        return Ok(None);
    }
    let Some(host) = asked_host(request) else {
        return Ok(None);
    };
    let Some(file) = file::file(env)? else {
        return Ok(None);
    };
    let token = file::secret_for(&file, &host)?;
    Ok(token.map(|token| HelperAnswer { token }))
}

/// The login host `request` asks a credential for: its `host` as a [`host_key`] under its
/// `protocol`. `None` when either key is missing or not UTF-8, the protocol is not `https` or
/// `http`, the host has no key, or the protocol is `http` and the host no loopback address.
fn asked_host(request: &[u8]) -> Option<String> {
    let (mut protocol, mut host) = (None, None);
    for line in request.split(|byte| *byte == b'\n') {
        // A blank line ends the request; git writes none before it.
        if line.is_empty() {
            break;
        }
        let at = line.iter().position(|byte| *byte == b'=');
        let Some((key, value)) = at.map(|at| (&line[..at], &line[at + 1..])) else {
            continue;
        };
        match key {
            b"protocol" => protocol = Some(value),
            b"host" => host = Some(value),
            _ => {}
        }
    }
    let https = match protocol? {
        b"https" => true,
        b"http" => false,
        _ => return None,
    };
    let key = host_key(std::str::from_utf8(host?).ok()?, https)?;
    (https || is_loopback(&key)).then_some(key)
}

/// Whether the host of `key`, a [`host_key`], is a literal loopback address.
fn is_loopback(key: &str) -> bool {
    let address = match key.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => key.split(':').next().unwrap_or_default(),
    };
    address.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use camino::Utf8Path;
    use tempfile::TempDir;

    use super::*;
    use crate::auth::AUTH_FILE_ENV;

    /// Tokens no provider issued, in no provider's shape.
    const TOKEN: &str = "example-token-not-a-secret";
    const LOOPBACK_TOKEN: &str = "loopback-token-not-a-secret";

    /// A login for github.com, one for a loopback port, one for a name at a port, and one of a
    /// provider no build knows.
    const LOGINS: &str = "version = 1\n\
        [[login]]\nprovider = \"gh\"\nhost = \"github.com\"\n\
        token = \"example-token-not-a-secret\"\nsource = \"with_token\"\n\
        [[login]]\nprovider = \"gh\"\nhost = \"127.0.0.1:8080\"\n\
        token = \"loopback-token-not-a-secret\"\nsource = \"with_token\"\n\
        [[login]]\nprovider = \"gh\"\nhost = \"plain.example:8080\"\n\
        token = \"plain-token-not-a-secret\"\nsource = \"with_token\"\n\
        [[login]]\nprovider = \"so\"\nhost = \"cloud.example\"\n\
        token = \"cloud-token-not-a-secret\"\n";

    /// A login file holding `text`, mode 0600, and the environment that names it.
    fn logins(text: &str) -> (TempDir, impl Fn(&str) -> Option<OsString>) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = Utf8Path::from_path(dir.path())
            .expect("UTF-8")
            .join("auth.toml");
        fs::write(&path, text).expect("write the file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("set the mode");
        let named = path.into_string();
        let env = move |name: &str| (name == AUTH_FILE_ENV).then(|| OsString::from(&named));
        (dir, env)
    }

    /// The lines an answer writes.
    fn written(answer: &HelperAnswer) -> String {
        let mut out = Vec::new();
        answer.write_to(&mut out).expect("write to memory");
        String::from_utf8(out).expect("UTF-8")
    }

    #[test]
    fn get_answers_a_login_s_host_with_two_lines() {
        let (_dir, env) = logins(LOGINS);
        let get = |request: &str| git_credential("get", request.as_bytes(), &env).unwrap();

        // 1. git's request for an https URL, with keys this build does not know.
        let request = "capability[]=authtype\ncapability[]=state\nprotocol=https\n\
                       host=github.com\nwwwauth[]=Basic realm=\"GitHub\"\n\n";
        let answer = get(request).expect("an answer");
        let lines = format!("username=x-access-token\npassword={TOKEN}\n");
        assert_eq!(written(&answer), lines);
        assert_eq!(format!("{answer:?}"), "HelperAnswer(<redacted>)");

        // 2. The same host as git may spell it: its own port, another case, no blank line, a
        //    path, a user name it already has.
        for request in [
            "protocol=https\nhost=github.com:443\n",
            "protocol=https\nhost=GitHub.com",
            "protocol=https\nhost=github.com\npath=acme/repo.git\nusername=ada\n\n",
            "host=example.com\nprotocol=https\nhost=github.com\n\n",
        ] {
            let answer = get(request).unwrap_or_else(|| panic!("no answer to {request:?}"));
            assert_eq!(written(&answer), lines, "{request:?}");
        }

        // 3. A loopback address over http, at its port.
        let answer = get("protocol=http\nhost=127.0.0.1:8080\n\n").expect("an answer");
        let lines = format!("username=x-access-token\npassword={LOOPBACK_TOKEN}\n");
        assert_eq!(written(&answer), lines);
    }

    #[test]
    fn a_request_no_login_is_for_has_no_answer() {
        let (_dir, env) = logins(LOGINS);
        for request in [
            // Another host, a subdomain, a name that ends in the login's, another port.
            "protocol=https\nhost=example.com\n\n",
            "protocol=https\nhost=gist.github.com\n\n",
            "protocol=https\nhost=evilgithub.com\n\n",
            "protocol=https\nhost=github.com:8443\n\n",
            "protocol=https\nhost=127.0.0.1:8081\n\n",
            // Another protocol: ssh, http to a name, http to github.com at https's port.
            "protocol=ssh\nhost=github.com\n\n",
            "protocol=http\nhost=github.com\n\n",
            "protocol=http\nhost=github.com:443\n\n",
            "protocol=http\nhost=plain.example:8080\n\n",
            "protocol=HTTPS\nhost=github.com\n\n",
            // A login of a provider this build does not know.
            "protocol=https\nhost=cloud.example\n\n",
            // No host, no protocol, nothing, or the keys after the blank line that ends it.
            "protocol=https\n\n",
            "host=github.com\n\n",
            "",
            "\nprotocol=https\nhost=github.com\n",
            "path=x\n\nprotocol=https\nhost=github.com\n",
            // A host that has no key.
            "protocol=https\nhost=github.com/acme\n\n",
            "protocol=https\nhost=ada@github.com\n\n",
            "protocol=https\nhost=github.com:\n\n",
            "protocol=https\nhost=\n\n",
            "protocol=https\nhost=github.com\r\n\r\n",
        ] {
            let answer = git_credential("get", request.as_bytes(), &env).unwrap();
            assert!(answer.is_none(), "an answer to {request:?}");
        }
        let not_text = b"protocol=https\nhost=github.com\xff\n\n";
        assert!(git_credential("get", not_text, &env).unwrap().is_none());

        // The https login for a name at a port is given over https alone.
        let named = "protocol=https\nhost=plain.example:8080\n\n";
        let answer = git_credential("get", named.as_bytes(), &env).unwrap();
        assert!(answer.is_some(), "no answer to {named:?}");
    }

    #[test]
    fn store_erase_and_any_other_word_do_nothing() {
        let (dir, env) = logins(LOGINS);
        let file = dir.path().join("auth.toml");
        let before = fs::read(&file).expect("read the file");
        let request = format!("protocol=https\nhost=github.com\npassword={TOKEN}\n\n");
        for operation in ["store", "erase", "GET", "", "capability", "get "] {
            let answer = git_credential(operation, request.as_bytes(), &env).unwrap();
            assert!(answer.is_none(), "an answer to {operation:?}");
        }
        assert_eq!(fs::read(&file).expect("read the file"), before);

        // They do not open the file either: one that would be refused is not.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("set the mode");
        for operation in ["store", "erase"] {
            let answer = git_credential(operation, request.as_bytes(), &env).unwrap();
            assert!(answer.is_none(), "an answer to {operation:?}");
        }
    }

    #[test]
    fn a_refused_file_is_its_own_error_only_when_it_is_needed() {
        let (dir, env) = logins(LOGINS);
        let file = dir.path().join("auth.toml");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("set the mode");

        // 1. A get for a host that could have a login opens the file, and fails on it.
        for request in [
            "protocol=https\nhost=github.com\n\n",
            "protocol=https\nhost=example.com\n\n",
        ] {
            let error = git_credential("get", request.as_bytes(), &env).unwrap_err();
            assert_eq!(error.code, "E_INVALID_ARG", "{request:?}");
            assert_eq!(error.details["reason"], "auth_file_mode", "{request:?}");
            assert!(!format!("{error:?}").contains("not-a-secret"), "{error:?}");
        }

        // 2. A get no login can be for does not open it.
        for request in ["protocol=ssh\nhost=github.com\n\n", "protocol=https\n\n"] {
            let answer = git_credential("get", request.as_bytes(), &env).unwrap();
            assert!(answer.is_none(), "an answer to {request:?}");
        }

        // 3. No login file at all, or a host that names no place for one: no answer.
        let request = b"protocol=https\nhost=github.com\n\n";
        fs::remove_file(&file).expect("remove the file");
        assert!(git_credential("get", request, &env).unwrap().is_none());
        let nowhere = |_: &str| None;
        assert!(git_credential("get", request, &nowhere).unwrap().is_none());
    }

    #[test]
    fn loopback_is_a_literal_address() {
        for key in [
            "127.0.0.1",
            "127.0.0.1:8080",
            "127.1.2.3:1",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(is_loopback(key), "{key}");
        }
        for key in [
            "localhost",
            "localhost:8080",
            "github.com",
            "10.0.0.1:80",
            "[::2]:8080",
        ] {
            assert!(!is_loopback(key), "{key}");
        }
    }
}
