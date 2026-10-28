//! Host git as the server's child (M14 design §0.1): `upload-pack`, `receive-pack` or
//! `http-backend` on `serve.git`, built by [`GitCli`]'s one builder, its configuration in the
//! environment.
//!
//! ```text
//! client ══ stdin/stdout ══▶ git <service> (GIT_DIR = serve.git, config in GIT_CONFIG_*)
//!                                 │ stderr
//!                                 ▼
//!                       the store's paths masked ──▶ serve's stderr
//! ```

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::process::{ExitStatus, Stdio};

use super::repo::{ServeError, ServeRepo};
use crate::transport::GitCli;

/// What `upload-pack` serves: no partial clone filter (M14 design §1), and no object that no
/// served ref reaches, so that a refused push's objects are never fetched.
const UPLOAD_CONFIG: [(&str, &str); 2] = [
    ("uploadpack.allowFilter", "false"),
    ("uploadpack.allowAnySHA1InWant", "false"),
];

/// What `receive-pack` takes (M14 design §6): every command handed to the `proc-receive` hook,
/// every object checked, and no atomic push, push options or shallow update advertised.
const RECEIVE_CONFIG: [(&str, &str); 5] = [
    ("receive.procReceiveRefs", "refs/"),
    ("receive.fsckObjects", "true"),
    ("receive.advertiseAtomic", "false"),
    ("receive.advertisePushOptions", "false"),
    ("receive.shallowUpdate", "false"),
];

/// What `http-backend` adds to both: a push without `REMOTE_USER` reaches the hook, where the
/// access file alone decides (M14 design D3).
const HTTP_CONFIG: [(&str, &str); 1] = [("http.receivepack", "true")];

/// The setting that points git at the serving hooks.
const HOOKS_PATH: &str = "core.hooksPath";

/// The variable that carries the client's protocol.
const GIT_PROTOCOL: &str = "GIT_PROTOCOL";

/// What a masked store path reads as in git's messages.
const STORE_MASK: &str = "<store>";

/// The store's directory above `serve.git`: `<store>/git/serve.git`.
const STORE_DEPTH: usize = 2;

/// The git service a connection runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Service {
    /// `git upload-pack`: fetch and clone.
    UploadPack,
    /// `git receive-pack`: push.
    ReceivePack,
    /// `git http-backend`: either, over smart HTTP.
    HttpBackend,
}

/// What the child gets besides its configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildEnv {
    /// The client's `GIT_PROTOCOL`, passed on as it came.
    pub git_protocol: Option<OsString>,
    /// Variables the child gets: the hook's `SWARMOPS_SERVE_*`, a CGI request's metavariables.
    pub vars: BTreeMap<String, OsString>,
    /// Configuration the caller adds after the service's own, which it wins over:
    /// `receive.maxInputSize`.
    pub config: Vec<(String, OsString)>,
}

impl ChildEnv {
    /// No protocol, no variables and no added configuration.
    pub fn new() -> ChildEnv {
        ChildEnv::default()
    }
}

/// Runs `service` on `repo` with `env`, its stdin and stdout the connection's; its exit. git's
/// stderr reaches this process's with the store's path masked.
pub fn run(
    git: &GitCli,
    service: Service,
    repo: &ServeRepo,
    env: &ChildEnv,
) -> Result<ExitStatus, ServeError> {
    // 1. The configuration: the service's, the hooks, then the caller's.
    let hooks = OsString::from(repo.hooks_dir().as_str());
    let fixed = service_config(service)
        .into_iter()
        .map(|(key, value)| (key, OsStr::new(value)));
    let added = (env.config.iter()).map(|(key, value)| (key.as_str(), value.as_os_str()));
    let config: Vec<(&str, &OsStr)> = fixed
        .chain([(HOOKS_PATH, hooks.as_os_str())])
        .chain(added)
        .collect();

    // 2. The command: the service on `.` (the working directory is serve.git), the client's
    //    protocol or none, the caller's variables, the connection's stdin and stdout.
    let mut command = git.serve_command(repo.git_dir().as_std_path(), &config);
    command.args(service_args(service));
    match &env.git_protocol {
        Some(protocol) => command.env(GIT_PROTOCOL, protocol),
        None => command.env_remove(GIT_PROTOCOL),
    };
    command
        .envs(&env.vars)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());

    // 3. Run it to its end, its stderr masked on the way.
    let mut child = command.spawn().map_err(|e| failed(&e))?;
    if let Some(stderr) = child.stderr.take() {
        copy_masked(stderr, &mut io::stderr(), &masks(repo));
    }
    child.wait().map_err(|e| failed(&e))
}

/// The configuration `service` runs with, before the hooks path and the caller's.
fn service_config(service: Service) -> Vec<(&'static str, &'static str)> {
    match service {
        Service::UploadPack => UPLOAD_CONFIG.to_vec(),
        Service::ReceivePack => RECEIVE_CONFIG.to_vec(),
        Service::HttpBackend => {
            [&UPLOAD_CONFIG[..], &RECEIVE_CONFIG[..], &HTTP_CONFIG[..]].concat()
        }
    }
}

/// git's arguments for `service`. `upload-pack` and `receive-pack` take the repository, `.`,
/// strictly (no `.git` appended); `http-backend` reads its request from the environment.
fn service_args(service: Service) -> &'static [&'static str] {
    match service {
        Service::UploadPack => &["upload-pack", "--strict", "--end-of-options", "."],
        Service::ReceivePack => &["receive-pack", "--end-of-options", "."],
        Service::HttpBackend => &["http-backend"],
    }
}

/// The texts of the store's path git may print: as `serve.git` names it, and canonical.
fn masks(repo: &ServeRepo) -> Vec<String> {
    let Some(store) = repo.git_dir().ancestors().nth(STORE_DEPTH) else {
        return Vec::new();
    };
    let mut masks = vec![store.as_str().to_owned()];
    if let Ok(canonical) = store.canonicalize_utf8() {
        masks.push(canonical.into_string());
    }
    masks.retain(|mask| mask.len() > 1);
    masks.sort_by_key(|mask| std::cmp::Reverse(mask.len()));
    masks
}

/// Copies `from` to `to` line by line, each of `masks` replaced by [`STORE_MASK`]. A write that
/// fails ends the copy's writing, not its reading: git is never blocked on a full pipe.
fn copy_masked(from: impl Read, to: &mut impl Write, masks: &[String]) {
    let mut from = BufReader::new(from);
    let mut line = Vec::new();
    let mut writing = true;
    loop {
        line.clear();
        match from.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if !writing {
            continue;
        }
        let text = masks
            .iter()
            .fold(String::from_utf8_lossy(&line).into_owned(), |text, mask| {
                text.replace(mask.as_str(), STORE_MASK)
            });
        writing = to
            .write_all(text.as_bytes())
            .and_then(|()| to.flush())
            .is_ok();
    }
}

/// The error of a git child that did not start or could not be waited for.
fn failed(e: &io::Error) -> ServeError {
    ServeError::Io {
        what: "serve.git".to_owned(),
        reason: format!("running git: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Service, copy_masked, service_args, service_config};

    /// Each service's configuration: upload-pack's for reading, receive-pack's for pushing,
    /// both and `http.receivepack` behind a web server.
    #[test]
    fn each_service_gets_its_configuration() {
        let keys = |service| -> Vec<&str> {
            service_config(service)
                .into_iter()
                .map(|(key, _)| key)
                .collect()
        };
        assert_eq!(
            keys(Service::UploadPack),
            ["uploadpack.allowFilter", "uploadpack.allowAnySHA1InWant"]
        );
        let receive = keys(Service::ReceivePack);
        assert!(receive.contains(&"receive.procReceiveRefs"), "{receive:?}");
        assert!(receive.contains(&"receive.advertiseAtomic"), "{receive:?}");
        let http = keys(Service::HttpBackend);
        for key in keys(Service::UploadPack).into_iter().chain(receive) {
            assert!(http.contains(&key), "{key}: {http:?}");
        }
        assert!(http.contains(&"http.receivepack"), "{http:?}");
        assert_eq!(service_args(Service::UploadPack)[0], "upload-pack");
    }

    /// git's stderr reaches the client with the store's path masked, line by line, and a last
    /// line without its newline kept.
    #[test]
    fn stderr_is_masked() {
        let masks = ["/srv/so/acme".to_owned()];
        let input = b"fatal: /srv/so/acme/git/serve.git: broken\nlast /srv/so/acme" as &[u8];
        let mut out = Vec::new();
        copy_masked(input, &mut out, &masks);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "fatal: <store>/git/serve.git: broken\nlast <store>"
        );
    }
}
