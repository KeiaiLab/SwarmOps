//! `serve --cgi` (M14 design §2.2): smart HTTP behind a web server, through git's `http-backend`
//! on `serve.git`.
//!
//! ```text
//! PATH_INFO /acme.git/git-upload-pack ─▶ 1 store `acme`, request `git-upload-pack`
//! 2 store under --root ─▶ 3 access.toml: read ─▶ 4 git found ─▶ 5 memo key, serve.git
//!      refreshed (steps 2 to 5: the ssh fetch side's `prepare`)
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

use camino::Utf8Path;

use swarmops_api::{ErrorCode, Identity, OpError};
use swarmops_git::serve::child::{self, ChildEnv, Service};
use swarmops_git::serve::repo::ServeRepo;

use super::entry::store_under;
use super::guard::Redactor;
use super::receive::{MAX_PUSH_BYTES, RAYON_NUM_THREADS, SERVE_HOOK_THREADS};
use super::records::{Connection, SWARMOPS_SERVE_CONNECTION, Side};
use super::refresh::serve_error;
use super::upload::{SERVE_GIT, exit_code, prepare};
use super::{
    Entry, SWARMOPS_SERVE_EXE, SWARMOPS_SERVE_IDENTITY, SWARMOPS_SERVE_STORE, ServeInvocation,
    Served, refused, reply_then_step, step_store,
};

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
    // 3-5. The read, git, the memo key, serve.git refreshed.
    let prepared = prepare(invocation, who, &request.store, store, redactor);
    let (repo, git) = prepared.map_err(|e| redactor.redact_error(e))?;

    // 6. git http-backend on stdin and stdout: the request's variables but its paths, then
    //    serve.git as the one repository, and for a push the hook's variables.
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
        redactor,
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

/// [`parse_path`] for the fuzz targets: the store's name and its smart request, or the refusal.
#[cfg(feature = "fuzzing")]
pub fn fuzz_parse_path(path_info: &str) -> Result<(String, &'static str), OpError> {
    parse_path(path_info).map(|request| (request.store, request.rest))
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

    // 3. The push bound and the hooks' threads, as over ssh. A request's side is not asked:
    //    a fetch's pack-objects hook runs on the same threads, where over ssh it gets no bound.
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    use camino::Utf8Path;
    use swarmops_api::{Identity, OpError};
    use swarmops_git::serve::repo::ServeRepo;

    use super::{
        CgiRequest, MAX_INPUT_SIZE, MAX_PUSH_BYTES, PATH_INFO, PATH_TRANSLATED, RAYON_NUM_THREADS,
        SERVE_HOOK_THREADS, child_env, parse_path,
    };
    use crate::serve::{Entry, IdentitySource, ServeInvocation};

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

    /// What `http-backend` gets for a push and for a fetch alike: the hooks' threads and the
    /// push bound, as over ssh, `serve.git` as its path, and no host path of the web server's.
    #[test]
    fn child_env_bounds_hooks_and_push() {
        let dir = tempfile::tempdir().unwrap();
        let store = Utf8Path::from_path(dir.path()).unwrap();
        let repo = ServeRepo::open(store).unwrap();
        let who = Identity::parse("person/ada").unwrap();
        let vars = BTreeMap::from([
            (PATH_INFO.to_owned(), "/acme.git/x".to_owned()),
            (PATH_TRANSLATED.to_owned(), "/srv/www/acme.git/x".to_owned()),
            ("REQUEST_METHOD".to_owned(), "POST".to_owned()),
        ]);
        let entry = Entry::Cgi { vars: vars.clone() };
        let no_keys = Box::new(|_: &str| Ok(None));
        let invocation =
            ServeInvocation::new(store.to_owned(), entry, IdentitySource::Absent, no_keys);

        for (rest, path) in [
            ("git-receive-pack", "/serve.git/git-receive-pack"),
            ("git-upload-pack", "/serve.git/git-upload-pack"),
        ] {
            let request = parse_path(&format!("/acme.git/{rest}")).unwrap();
            let env = child_env(&invocation, &who, &vars, &repo, &request, store);

            // 1. The hooks' threads and the push bound.
            let threads = OsString::from(SERVE_HOOK_THREADS.to_string());
            assert_eq!(env.vars.get(RAYON_NUM_THREADS), Some(&threads), "{rest}");
            let bound = (
                MAX_INPUT_SIZE.to_owned(),
                OsString::from(MAX_PUSH_BYTES.to_string()),
            );
            assert!(env.config.contains(&bound), "{rest}: {:?}", env.config);

            // 2. The request's own variables, but its paths.
            assert_eq!(
                env.vars.get(PATH_INFO),
                Some(&OsString::from(path)),
                "{rest}"
            );
            assert_eq!(env.vars.get(PATH_TRANSLATED), None, "{rest}");
            assert_eq!(
                env.vars.get("REQUEST_METHOD"),
                Some(&OsString::from("POST"))
            );
        }
    }
}
