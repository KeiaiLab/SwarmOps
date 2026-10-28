//! The request of an ssh connection (M14 design §2.1): `SSH_ORIGINAL_COMMAND` split as git quotes
//! it, with no shell, into the git service and the store it names; and that store's directory
//! under `--root`.
//!
//! ```text
//! git-upload-pack '~/acme.git/'  ─▶  service UploadPack, path ~/acme.git/
//!                                ─▶  one `/` or `~/` off the front, one `/`, then `.git`, off
//!                                    the end: acme, which the remote-name grammar must take
//! ```

use camino::{Utf8Path, Utf8PathBuf};
use swarmops_api::{ErrorCode, OpError};
use swarmops_git::serve::child::Service;

use super::refused;

/// The command line of M11's store-to-store sync, which a forced command does not serve yet.
const STDIO_COMMAND: &str = "swarmops serve --stdio";

/// The commands a forced command serves, as git's clients spell them, each before one space and
/// the quoted path.
const SERVICES: [(&str, Service); 4] = [
    ("git-upload-pack", Service::UploadPack),
    ("git upload-pack", Service::UploadPack),
    ("git-receive-pack", Service::ReceivePack),
    ("git receive-pack", Service::ReceivePack),
];

/// The prefixes a path may start with, one of them: absolute, or under the account's home.
const PATH_PREFIXES: [&str; 2] = ["~/", "/"];

/// The suffix a repository path usually ends in.
const GIT_SUFFIX: &str = ".git";

/// What `serve` says of a path it does not take.
const BAD_PATH_HINT: &str =
    "name a store under the served root by its name alone: ssh://<host>/<name>.git";

/// What an ssh client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshRequest {
    /// `upload-pack` or `receive-pack`.
    pub service: Service,
    /// The store's name, its path stripped of `/`, `~/`, `.git` and a trailing `/`.
    pub store: String,
}

impl SshRequest {
    /// The request for `service` on the store named `store`.
    pub fn new(service: Service, store: impl Into<String>) -> SshRequest {
        SshRequest {
            service,
            store: store.into(),
        }
    }
}

/// The request `command` (`SSH_ORIGINAL_COMMAND`) makes. `swarmops serve --stdio` is
/// `E_INVALID_ARG` `not_served`: M11 serves it. Another command is `unknown_command`, and a path
/// that is not one store's name, quoted as git quotes it, `bad_path`.
pub fn parse_ssh(command: &str) -> Result<SshRequest, OpError> {
    // 1. M11's sync, refused by its own reason.
    let stdio = command
        .strip_prefix(STDIO_COMMAND)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '));
    if stdio {
        return Err(refused(
            ErrorCode::InvalidArg,
            "not_served",
            "swarmops serve --stdio, store-to-store sync, is not served yet",
        ));
    }

    // 2. The service, then its one argument, as git's sq_quote writes it.
    let found = SERVICES.iter().find_map(|(word, service)| {
        let rest = command.strip_prefix(word)?.strip_prefix(' ')?;
        Some((*service, rest))
    });
    let Some((service, quoted)) = found else {
        return Err(refused(
            ErrorCode::InvalidArg,
            "unknown_command",
            "serve runs git-upload-pack and git-receive-pack alone",
        )
        .with_hint("clone, fetch, pull or push with git over ssh"));
    };
    let path =
        dequote(quoted).ok_or_else(|| bad_path("the path is not quoted as git quotes it"))?;

    // 3. The store's name: the path without its usual decorations, one segment.
    let store = store_name(&path).ok_or_else(|| bad_path("the path names no store"))?;
    Ok(SshRequest::new(service, store))
}

/// The directory of the store named `name` under `root`, which must stay under it: its
/// canonical path. A name the remote-name grammar refuses, or a link out of the root, is
/// `E_INVALID_ARG` `bad_path`; no such directory `E_NOT_A_REPO`. No error names a host path.
pub fn store_under(root: &Utf8Path, name: &str) -> Result<Utf8PathBuf, OpError> {
    // 1. One segment of the grammar: no separator, no `..`.
    if swarmops_store::check_remote_segment(name).is_err() {
        return Err(bad_path("the path names no store"));
    }

    // 2. Both canonical, the store's below the root's.
    let missing = || {
        OpError::new(
            ErrorCode::NotARepo,
            format!("no store named {name} is served"),
        )
        .with_hint("ask the server's operator for the store's name")
    };
    let root = root.canonicalize_utf8().map_err(|_| {
        OpError::new(ErrorCode::NotARepo, "the served root is not there")
            .with_hint("the server's operator names an existing directory in --root")
    })?;
    let store = root.join(name).canonicalize_utf8().map_err(|_| missing())?;
    if store == root || !store.starts_with(&root) {
        return Err(bad_path("the store leaves the served root"));
    }
    Ok(store)
}

/// `quoted` without git's quoting: one `'…'` run, or several joined by `\'` or `\!` (git's
/// `sq_quote_buf` writes `'` as `'\''` and `!` as `'\!'`); `None` for anything else.
fn dequote(quoted: &str) -> Option<String> {
    let mut rest = quoted.strip_prefix('\'')?;
    let mut out = String::new();
    loop {
        // 1. A quoted run, to its closing quote.
        let end = rest.find('\'')?;
        out.push_str(&rest[..end]);
        rest = &rest[end + 1..];

        // 2. The end, or an escaped quote or bang and the next run.
        if rest.is_empty() {
            return Some(out);
        }
        let mut chars = rest.chars();
        let (Some('\\'), Some(escaped @ ('\'' | '!')), Some('\'')) =
            (chars.next(), chars.next(), chars.next())
        else {
            return None;
        };
        out.push(escaped);
        rest = chars.as_str();
    }
}

/// The store `path` names: one `/` or `~/` off its front, then one `/` and one `.git` off its
/// end; `None` unless what is left is one segment of the remote-name grammar.
fn store_name(path: &str) -> Option<String> {
    let path = PATH_PREFIXES
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix))
        .unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(GIT_SUFFIX).unwrap_or(path);
    swarmops_store::check_remote_segment(path).ok()?;
    Some(path.to_owned())
}

/// `E_INVALID_ARG` `bad_path`, saying `why`.
fn bad_path(why: &str) -> OpError {
    refused(ErrorCode::InvalidArg, "bad_path", why).with_hint(BAD_PATH_HINT)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use camino::Utf8PathBuf;
    use swarmops_api::OpError;
    use swarmops_git::serve::child::Service;

    use super::{SshRequest, parse_ssh, store_under};

    /// The reason an error gives in its details.
    fn reason(error: &OpError) -> &str {
        error.details["reason"].as_str().unwrap_or_default()
    }

    /// Both spellings of both services, and the forms git and people write a path in.
    #[test]
    fn parse_ssh_takes_both_spellings_and_git_quoting() {
        // 1. Each service, dashed or spaced.
        let services = [
            ("git-upload-pack", Service::UploadPack),
            ("git upload-pack", Service::UploadPack),
            ("git-receive-pack", Service::ReceivePack),
            ("git receive-pack", Service::ReceivePack),
        ];
        for (word, service) in services {
            let request = parse_ssh(&format!("{word} '/acme.git'")).unwrap();
            assert_eq!(request, SshRequest::new(service, "acme"), "{word}");
        }

        // 2. The path's forms: `/`, `~/`, `.git`, a trailing `/`, git's quoting of `'` and `!`.
        let paths = [
            ("'acme'", "acme"),
            ("'/acme'", "acme"),
            ("'~/acme.git'", "acme"),
            ("'/acme.git/'", "acme"),
            ("'acme/'", "acme"),
            ("'/a.b_c-1.git'", "a.b_c-1"),
            ("'a'\\''b'", "a'b"),
            ("'a'\\!'b'", "a!b"),
        ];
        for (quoted, store) in paths {
            let line = format!("git-upload-pack {quoted}");
            match parse_ssh(&line) {
                Ok(request) => assert_eq!(request.store, store, "{line}"),
                // A quote or `!` in a name: dequoted, then refused by the name's grammar.
                Err(error) => {
                    assert!(store.contains(['\'', '!']), "{line}: {error:?}");
                    assert_eq!(reason(&error), "bad_path", "{line}");
                }
            }
        }
    }

    /// A path that names no one store under the root is `bad_path`, before anything is opened.
    #[test]
    fn parse_ssh_refuses_bad_paths() {
        let bad = [
            "''",
            "'/'",
            "'..'",
            "'/../x'",
            "'../x.git'",
            "'/a/b'",
            "'a/b.git'",
            "'/srv/so/acme.git'",
            "'~/'",
            "'.git'",
            "'Acme'",
            "'a\0b'",
            "'acme",
            "acme",
            "'acme' 'other'",
            "'a'b'",
        ];
        for path in bad {
            let line = format!("git-upload-pack {path}");
            let error = parse_ssh(&line).unwrap_err();
            assert_eq!(error.code.as_str(), "E_INVALID_ARG", "{line:?}");
            assert_eq!(reason(&error), "bad_path", "{line:?}: {error:?}");
        }
    }

    /// What is no git service is `unknown_command`; M11's sync is `not_served`.
    #[test]
    fn parse_ssh_refuses_other_commands() {
        for line in [
            "",
            "ls",
            "git-upload-archive 'acme'",
            "git upload-pack",
            "sh -c 'x'",
        ] {
            let error = parse_ssh(line).unwrap_err();
            assert_eq!(error.code.as_str(), "E_INVALID_ARG", "{line:?}");
            assert_eq!(reason(&error), "unknown_command", "{line:?}: {error:?}");
        }
        let error = parse_ssh("swarmops serve --stdio").unwrap_err();
        assert_eq!(reason(&error), "not_served", "{error:?}");
    }

    /// A store under the root is its canonical directory; one that is missing is
    /// `E_NOT_A_REPO`, and a link that leaves the root `bad_path`, neither naming a host path.
    #[test]
    fn store_under_stays_under_the_root() {
        // 1. The root, a store in it, a link inside it, a link out of it, a store outside it.
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        for store in [root.join("acme"), outside.clone()] {
            std::fs::create_dir_all(store).unwrap();
        }
        symlink(root.join("acme"), root.join("alias")).unwrap();
        symlink(&outside, root.join("escape")).unwrap();

        // 2. The store, through its name and through the link that stays.
        let canonical = root.join("acme").canonicalize_utf8().unwrap();
        assert_eq!(store_under(&root, "acme").unwrap(), canonical);
        assert_eq!(store_under(&root, "alias").unwrap(), canonical);

        // 3. The refusals.
        let escape = store_under(&root, "escape").unwrap_err();
        assert_eq!(escape.code.as_str(), "E_INVALID_ARG", "{escape:?}");
        assert_eq!(reason(&escape), "bad_path", "{escape:?}");
        let missing = store_under(&root, "missing").unwrap_err();
        assert_eq!(missing.code.as_str(), "E_NOT_A_REPO", "{missing:?}");
        let invalid = store_under(&root, "..").unwrap_err();
        assert_eq!(reason(&invalid), "bad_path", "{invalid:?}");
        for error in [escape, missing, invalid] {
            let text = serde_json::to_string(&error).unwrap();
            assert!(!text.contains(base.as_str()), "{text}");
        }
    }
}
