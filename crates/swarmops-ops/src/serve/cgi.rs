//! `serve --cgi` (M14 design §2.2): smart HTTP behind a web server, through git's `http-backend`
//! on `serve.git`.
//!
//! ```text
//! PATH_INFO /acme.git/git-upload-pack ─▶ 1 store `acme`, request `git-upload-pack`
//! 2 store under --root ─▶ 3 memo key ─▶ 4 access.toml: read ─▶ 5 serve.git refreshed
//! 6 git http-backend: GIT_PROJECT_ROOT=<store>/git, PATH_INFO=/serve.git/git-upload-pack
//!      ── receive-pack ──▶ proc-receive hook (SWARMOPS_SERVE_EXE · _STORE · _IDENTITY)
//! 7 its record, from step 2 on: git/serve-records.jsonl
//! ```
//!
//! The web server terminates TLS and authenticates; `serve` names the caller (`--as`, or
//! `--user-family` with `REMOTE_USER`) before this module runs. Every request needs the read
//! grant; what a push may move is the hook's to decide, ref by ref, as over ssh. git gets
//! `http.receivepack=true`, so that a push without `REMOTE_USER` reaches the hook and the access
//! file alone decides (design D3). Each step before git starts refuses with no response byte
//! written, its texts masked by the [`Redactor`] of `--root`; the CLI answers the refusal with
//! its HTTP status.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

use camino::Utf8Path;

use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_git::serve::child::{self, ChildEnv, Service};
use swarmops_git::serve::repo::ServeRepo;
use swarmops_git::transport::GitCli;

use super::access::{Access, Action};
use super::entry::store_under;
use super::guard::Redactor;
use super::receive::{MAX_PUSH_BYTES, RAYON_NUM_THREADS, SERVE_HOOK_THREADS};
use super::records::{Connection, SWARMOPS_SERVE_CONNECTION, Side};
use super::refresh::{Freshness, Refreshed, Scope, freshness, refresh, serve_error};
use super::{
    Entry, SWARMOPS_SERVE_EXE, SWARMOPS_SERVE_IDENTITY, SWARMOPS_SERVE_STORE, ServeInvocation,
    Served, refused, reply_then_step, step_store,
};
use crate::LocalOps;
use crate::error;

/// The CGI metavariable of the request's path below the script.
const PATH_INFO: &str = "PATH_INFO";

/// The metavariable a web server may name a host path in, which `http-backend` never gets.
const PATH_TRANSLATED: &str = "PATH_TRANSLATED";

/// What `http-backend` reads the repositories' directory from.
const GIT_PROJECT_ROOT: &str = "GIT_PROJECT_ROOT";

/// What lets `http-backend` serve a repository without a `git-daemon-export-ok` file.
const GIT_HTTP_EXPORT_ALL: &str = "GIT_HTTP_EXPORT_ALL";

/// The requests of git's smart HTTP, after the store's segment.
const SMART_PATHS: [&str; 3] = ["info/refs", "git-upload-pack", "git-receive-pack"];

/// The CGI metavariable of the request's query.
const QUERY_STRING: &str = "QUERY_STRING";

/// The smart HTTP requests of a push: its advertisement's query, and the push itself.
const RECEIVE_ADVERTISEMENT: &str = "service=git-receive-pack";
const RECEIVE_PACK: &str = "git-receive-pack";
const INFO_REFS: &str = "info/refs";

/// The suffix a repository path usually ends in.
const GIT_SUFFIX: &str = ".git";

/// git's setting that bounds what a push sends.
const MAX_INPUT_SIZE: &str = "receive.maxInputSize";

/// The name the texts give the serving repository: never its host path.
const SERVE_GIT: &str = "serve.git";

/// The exit of a git killed by a signal: the shell's `128 + signal`.
const SIGNAL_EXIT: i32 = 128;

/// What `serve` says of a path it does not take.
const PATH_HINT: &str = "clone, fetch or push https://<host>/<prefix>/<name>.git with git";

/// What a CGI request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CgiRequest {
    /// The store's name, its segment without `.git`.
    store: String,
    /// One of [`SMART_PATHS`].
    rest: &'static str,
}

/// Serves the CGI request of `invocation` to `who`.
pub fn run(invocation: &ServeInvocation, who: &Identity) -> Result<Served, OpError> {
    let redactor = Redactor::new(&[invocation.root.as_std_path()]);
    let Entry::Cgi { vars } = &invocation.entry else {
        return Err(OpError::new(
            ErrorCode::InvalidArg,
            "serve --cgi runs on a CGI request alone",
        ));
    };

    // 1-2. The request and the store; from here on, the request is recorded.
    let connection = Connection::start();
    let path_info = vars.get(PATH_INFO).map_or("", String::as_str);
    let request = parse_path(path_info)?;
    let store = store_under(&invocation.root, &request.store);
    let store = store.map_err(|e| redactor.redact_error(e))?;
    let served = serve(
        invocation,
        who,
        vars,
        &request,
        &store,
        &connection,
        &redactor,
    );
    connection.finish(&store, who, side_of(&request, vars), &served, &redactor);
    served
}

/// Serves `request` of the store at `store`, with the metavariables `vars`, to `who` on
/// `connection`.
fn serve(
    invocation: &ServeInvocation,
    who: &Identity,
    vars: &BTreeMap<String, String>,
    request: &CgiRequest,
    store: &Utf8Path,
    connection: &Connection,
    redactor: &Redactor,
) -> Result<Served, OpError> {
    // 3-5. The memo key, the read, serve.git refreshed.
    let repo = prepare(invocation, who, request, store, redactor);
    let repo = repo.map_err(|e| redactor.redact_error(e))?;

    // 6. git http-backend on stdin and stdout: the request's variables but its paths, then
    //    serve.git as the one repository, and for a push the hook's variables.
    let git = GitCli::find().map_err(|e| git_error(e, redactor))?;
    let mut env = child_env(invocation, who, vars, &repo, request, store);
    env.vars.insert(
        SWARMOPS_SERVE_CONNECTION.to_owned(),
        OsString::from(connection.id()),
    );
    //    Once git has sent its reply, a push's commits get their view work stepped; a fetch
    //    committed nothing.
    let side = side_of(request, vars);
    let status = reply_then_step(
        || child::run(&git, Service::HttpBackend, &repo, &env),
        || {
            if side == Side::Push {
                return step_store(store);
            }
            Ok(())
        },
        &mut std::io::stderr(),
    );
    let status = status.map_err(|e| redactor.redact_error(serve_error(e)))?;
    Ok(Served::new(exit_code(status)))
}

/// The side `request` serves: a push's `git-receive-pack`, and its advertisement
/// (`info/refs?service=git-receive-pack`); else a fetch's.
fn side_of(request: &CgiRequest, vars: &BTreeMap<String, String>) -> Side {
    let query = vars.get(QUERY_STRING).map_or("", String::as_str);
    let advertised = query.split('&').any(|pair| pair == RECEIVE_ADVERTISEMENT);
    if request.rest == RECEIVE_PACK || (request.rest == INFO_REFS && advertised) {
        return Side::Push;
    }
    Side::Fetch
}

/// The request `path_info` makes: `/<name>[.git]/<smart path>`. A first segment that is no
/// store's name is `E_INVALID_ARG` `bad_path` (400); another path `E_NOT_A_REPO`
/// `not_smart_http` (404).
fn parse_path(path_info: &str) -> Result<CgiRequest, OpError> {
    // 1. The first segment, which must name a store.
    let path = path_info.strip_prefix('/').unwrap_or(path_info);
    let Some((segment, rest)) = path.split_once('/') else {
        return Err(not_smart_http());
    };
    let store = segment.strip_suffix(GIT_SUFFIX).unwrap_or(segment);
    if swarmops_store::check_remote_segment(store).is_err() {
        return Err(refused(
            ErrorCode::InvalidArg,
            "bad_path",
            "the path names no store under the served root",
        )
        .with_hint(PATH_HINT));
    }

    // 2. The rest, one of git's smart HTTP requests.
    let Some(rest) = SMART_PATHS.iter().find(|smart| **smart == rest) else {
        return Err(not_smart_http());
    };
    Ok(CgiRequest {
        store: store.to_owned(),
        rest,
    })
}

/// `E_NOT_A_REPO` `not_smart_http`: a path that is none of git's smart HTTP requests.
fn not_smart_http() -> OpError {
    refused(
        ErrorCode::NotARepo,
        "not_smart_http",
        "serve answers git's smart HTTP alone: <name>.git/info/refs, git-upload-pack and \
         git-receive-pack",
    )
    .with_hint(PATH_HINT)
}

/// Steps 2 to 5: the store at `store`, which `request` names, opened with its memo key, `who`'s
/// read checked, and its `serve.git` refreshed (its hook made or replaced on the way); what the
/// refresh warns of goes to stderr, the web server's log, masked by `redactor`. The serving
/// repository; the store is closed again before git starts.
fn prepare(
    invocation: &ServeInvocation,
    who: &Identity,
    request: &CgiRequest,
    store: &Utf8Path,
    redactor: &Redactor,
) -> Result<ServeRepo, OpError> {
    // 2. The store, which the request named under the root.
    let ops = LocalOps::open(store)?;

    // 3. Its memo key, kept by the serving account outside the store.
    let repo_id = ops.repo.info().map_err(error::op)?.repo_id;
    let ops = match (invocation.memo_keys)(&repo_id)? {
        Some(key) => ops.with_memo_key(key),
        None => ops,
    };

    // 4. The caller may read it: what it may push is the hook's to decide, ref by ref.
    Access::load(&ops)?.check(who, Action::Read, &request.store)?;

    // 5. Its serving repository, refreshed from the committed snapshot,
    //    unless nothing moved since its last export (no lock taken then).
    let repo = ServeRepo::open(store).map_err(serve_error)?;
    let refreshed = match freshness(&ops, &repo)? {
        Freshness::Current => Refreshed::default(),
        Freshness::Stale => refresh(&ops, &repo, &Scope::All)?,
    };
    let mut stderr = std::io::stderr().lock();
    for warning in refreshed.warnings {
        let _ = writeln!(stderr, "warning: {}", redactor.redact(&warning));
    }
    Ok(repo)
}

/// What `http-backend` gets: the request's metavariables but its paths, `serve.git` as the
/// project root's one repository, the hook's variables, the push bound and the hooks' threads.
fn child_env(
    invocation: &ServeInvocation,
    who: &Identity,
    vars: &BTreeMap<String, String>,
    repo: &ServeRepo,
    request: &CgiRequest,
    store: &Utf8Path,
) -> ChildEnv {
    // 1. The request: its method, query, body's type and length, protocol, user.
    let mut env = ChildEnv::new();
    let passed = vars
        .iter()
        .filter(|(name, _)| ![PATH_INFO, PATH_TRANSLATED].contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), OsString::from(value)));
    env.vars.extend(passed);

    // 2. The repository: <store>/git/serve.git, as /serve.git under <store>/git.
    let git_dir = repo.git_dir();
    let project_root = git_dir.parent().unwrap_or(git_dir);
    let name = git_dir.file_name().unwrap_or(SERVE_GIT);
    let vars = [
        (GIT_PROJECT_ROOT, OsString::from(project_root.as_str())),
        (
            PATH_INFO,
            OsString::from(format!("/{name}/{}", request.rest)),
        ),
        (GIT_HTTP_EXPORT_ALL, OsString::from("1")),
        (SWARMOPS_SERVE_STORE, OsString::from(store.as_str())),
        (SWARMOPS_SERVE_IDENTITY, OsString::from(who.agent())),
    ];
    for (key, value) in vars {
        env.vars.insert(key.to_owned(), value);
    }
    if let Some(exe) = &invocation.exe {
        env.vars
            .insert(SWARMOPS_SERVE_EXE.to_owned(), exe.clone().into_os_string());
    }

    // 3. The push bound and the hooks' threads, as over ssh.
    env.config.push((
        MAX_INPUT_SIZE.to_owned(),
        OsString::from(MAX_PUSH_BYTES.to_string()),
    ));
    env.vars.insert(
        RAYON_NUM_THREADS.to_owned(),
        OsString::from(SERVE_HOOK_THREADS.to_string()),
    );
    env
}

/// The error of finding no git to run, masked.
fn git_error(e: swarmops_git::transport::TransportError, redactor: &Redactor) -> OpError {
    redactor.redact_error(error::transport_error(e, SERVE_GIT, error::Phase::Fetch))
}

/// The code a request exits with: git's, or `128 + signal` for a git a signal ended.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT + signal))
        .unwrap_or(SIGNAL_EXIT)
}

#[cfg(test)]
mod tests {
    use swarmops_api::OpError;

    use super::{CgiRequest, parse_path};

    /// The reason an error gives in its details.
    fn reason(error: &OpError) -> &str {
        error.details["reason"].as_str().unwrap_or_default()
    }

    /// The three smart HTTP requests, under a store's name with or without `.git`.
    #[test]
    fn parse_path_takes_smart_requests() {
        for (path, store, rest) in [
            ("/acme.git/info/refs", "acme", "info/refs"),
            ("/acme/git-upload-pack", "acme", "git-upload-pack"),
            (
                "/a.b_c-1.git/git-receive-pack",
                "a.b_c-1",
                "git-receive-pack",
            ),
        ] {
            let request = parse_path(path).unwrap();
            let expected = CgiRequest {
                store: store.to_owned(),
                rest,
            };
            assert_eq!(request, expected, "{path}");
        }
    }

    /// A first segment that names no store is `bad_path`; any other path is `not_smart_http`.
    #[test]
    fn parse_path_refuses_other_paths() {
        for path in [
            "/../x/info/refs",
            "/..git/info/refs",
            "/.git/info/refs",
            "//info/refs",
            "/Acme/info/refs",
            "/a%2Fb/info/refs",
        ] {
            let error = parse_path(path).unwrap_err();
            assert_eq!(error.code.as_str(), "E_INVALID_ARG", "{path}");
            assert_eq!(reason(&error), "bad_path", "{path}");
        }
        for path in [
            "",
            "/",
            "/acme.git",
            "/acme.git/",
            "/acme.git/HEAD",
            "/acme.git/objects/info/packs",
            "/acme.git/info/refs/x",
            "/acme/inner.git/info/refs",
        ] {
            let error = parse_path(path).unwrap_err();
            assert_eq!(error.code.as_str(), "E_NOT_A_REPO", "{path:?}");
            assert_eq!(reason(&error), "not_smart_http", "{path:?}");
        }
    }
}
