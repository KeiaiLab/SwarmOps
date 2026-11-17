//! A login's token on the way to a git remote, through the binary: `clone`, `fetch`, `push`,
//! `sync` and `remote add` send the token of the login kept for the remote's host to a remote
//! that names no `--token-env`, to that host alone, and never show it.
//!
//! The remote is the test kit's [`GitServer`] on `127.0.0.1`, behind HTTP basic authentication
//! that takes one token. `auth login --gh` keeps a login for `github.com` only, so each test
//! writes the login file itself, for the server's `127.0.0.1:<port>`, and names it with
//! `SWARMOPS_AUTH_FILE`; the binary reads it as it reads one that `auth login` wrote. The tokens
//! are placeholders in no provider's shape.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::Value;
use swarmops_testkit::remote::{GitServer, HttpAuth, HttpServer};
use tempfile::TempDir;

use crate::support::{Run, Store, run};

/// The agent the commands that need an identity run as.
const AGENT: &str = "agent/worker-17";

/// The repository every test serves.
const REPO: &str = "repo";

/// The remote's name in the store.
const ORIGIN: &str = "origin";

/// The user name a token goes with.
const USER: &str = "x-access-token";

/// The token the server takes.
const TOKEN: &str = "example-token-not-a-secret";

/// A token the server refuses.
const STALE: &str = "stale-token-not-a-secret";

/// The token of a login for another host, which must reach no server.
const ELSEWHERE: &str = "elsewhere-token-not-a-secret";

/// A token variable a remote names.
const VAR: &str = "SO_TEST_AUTH_TOKEN";

/// No `Authorization` header: what a server that was sent no token received.
const NO_HEADERS: [&str; 0] = [];

/// The variable that names the login file.
const AUTH_FILE: &str = "SWARMOPS_AUTH_FILE";

/// A served repository with two commits on `main` behind basic authentication, and what the
/// binary runs with: the server's git, an empty `HOME`, no system or global configuration, and
/// a login file of the test's own.
struct Site {
    /// The remote's side.
    server: GitServer,
    /// The server's smart HTTP, which takes [`TOKEN`].
    http: HttpServer,
    /// git's empty `HOME` and the login file's directory, removed when the site is dropped.
    home: TempDir,
    /// The variables every run sets.
    env: Vec<(String, String)>,
}

impl Site {
    fn new() -> Site {
        let server = GitServer::new();
        server.create(REPO);
        server.commit_on(REPO, "main", "first", &[("a.txt", "a\n")]);
        server.commit_on(REPO, "main", "second", &[("b.txt", "b\n")]);
        let http = server.http(HttpAuth::Basic {
            user: USER.to_owned(),
            token: TOKEN.to_owned(),
        });
        let home = tempfile::tempdir().expect("a temp HOME");
        let file = home.path().join("logins").join("auth.toml");
        let env = vec![
            ("SWARMOPS_GIT".to_owned(), path_text(&real_git(&server))),
            ("HOME".to_owned(), path_text(home.path())),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            (AUTH_FILE.to_owned(), path_text(&file)),
        ];
        Site {
            server,
            http,
            home,
            env,
        }
    }

    /// The login file's path: nothing is there until [`Site::log_in`] writes it.
    fn file(&self) -> PathBuf {
        self.home.path().join("logins").join("auth.toml")
    }

    /// Writes the login file, mode 0600, with one `gh` login per `(host, token)` of `logins`.
    fn log_in(&self, logins: &[(&str, &str)]) {
        let mut text = "version = 1\n".to_owned();
        for (host, token) in logins {
            write!(
                text,
                "\n[[login]]\nprovider = \"gh\"\nhost = \"{host}\"\ntoken = \"{token}\"\n\
                 source = \"with_token\"\n"
            )
            .expect("write to a string");
        }
        let file = self.file();
        fs::create_dir_all(file.parent().expect("a directory")).expect("make the directory");
        fs::write(&file, text).expect("write the login file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("set the mode");
    }

    /// The server's host as a login names it: `127.0.0.1:<port>`.
    fn host(&self) -> String {
        self.http.host()
    }

    /// The same address with another port.
    fn other_port(&self) -> String {
        let host = self.host();
        let (address, port) = host.rsplit_once(':').expect("a port");
        let port: u16 = port.parse().expect("a port number");
        format!(
            "{address}:{}",
            if port == u16::MAX { port - 1 } else { port + 1 }
        )
    }

    fn url(&self) -> String {
        self.http.url(REPO)
    }

    /// The variables every run sets.
    fn vars(&self) -> Vec<(&str, &str)> {
        let own = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        own.collect()
    }

    /// Runs the binary with `args` and the site's variables.
    fn run(&self, args: &[&str]) -> Run {
        run(args, &self.vars())
    }

    /// A fresh store whose commands run with the site's variables, [`ORIGIN`] recorded with
    /// `--branch main` (no network call) and then `more`.
    fn store(&self, more: &[&str]) -> Store {
        let store = Store::init().with_env(&self.vars());
        let url = self.url();
        let mut line = vec!["remote", "add", ORIGIN, url.as_str(), "--branch", "main"];
        line.extend_from_slice(more);
        store.read(&line).ok();
        store
    }

    /// The `Authorization` header a request with `token` carries.
    fn header(token: &str) -> String {
        format!("Basic {}", base64(&format!("{USER}:{token}")))
    }
}

/// `text` in standard padded base64, as HTTP's Basic scheme carries a credential.
fn base64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// The real git the server runs, as an absolute path.
fn real_git(server: &GitServer) -> PathBuf {
    let program = server.git_program();
    if program.is_absolute() {
        return program.to_path_buf();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// `path` as text.
fn path_text(path: &Path) -> String {
    path.to_str().expect("UTF-8 temp paths").to_owned()
}

/// Every file under `root`.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("a readable dir") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            files.push(path);
        }
    }
    files
}

/// Commits `content` at `path` on `main` of `store` and moves `main` to it.
fn land(store: &Store, path: &str, content: &str) {
    let refs = store.read(&["ref", "list"]).ok();
    let listed = refs["refs"].as_array().expect("refs");
    let main = listed.iter().find(|entry| entry["name"] == "main");
    let old = main.expect("main")["target"].as_str().expect("a target");
    let new = store.commit(AGENT, path, content, Some(old));
    let line = ["ref", "set", "main", new.as_str(), "--expect", old];
    store.run_as(AGENT, &line, &[]).ok();
}

/// Asserts that `refused` is the remote's refusal for the credential, and gives its error.
fn auth_refusal(refused: &Run) -> Value {
    let error = refused.err("E_REMOTE_REJECTED");
    assert_eq!(error["exit"], 5, "{refused}");
    assert_eq!(error["details"]["reason"], "auth", "{refused}");
    let hint = error["hint"].as_str().expect("a hint");
    assert!(hint.contains("swarmops auth status"), "{hint}");
    error
}

/// Asserts that neither stream of `done` holds `token`, as it is or in the header's base64.
fn assert_unseen(done: &Run, token: &str) {
    let encoded = base64(&format!("{USER}:{token}"));
    for text in [&done.stdout, &done.stderr] {
        assert!(!text.contains(token) && !text.contains(&encoded), "{done}");
    }
}

/// The login for the remote's host is what every command that reaches the remote sends, and
/// what none of them shows or writes: with no login the clone is refused for the credential;
/// with one, `clone`, `remote add` (asking for the default branch), `fetch`, `push` and `sync`
/// succeed, every request carrying that token; the login gone, the fetch is refused again.
#[test]
fn a_login_is_used_by_clone_fetch_push_and_sync() {
    let site = Site::new();
    let url = site.url();
    let dir = tempfile::tempdir().expect("a temp dir");
    let at = |name: &str| path_text(&dir.path().join(name));

    // 1. No login: the server answers 401 and git has nothing to answer with. The hint names
    //    the login, and the error does not say one was used.
    let (early, into) = (at("early"), at("clone"));
    let refused = site.run(&["--as", AGENT, "clone", &url, "--into", &early]);
    let error = auth_refusal(&refused);
    assert!(error["details"].get("login_used").is_none(), "{refused}");
    assert_eq!(site.http.authorizations(), NO_HEADERS);

    // 2. A login for the server's host, beside one for another host: every remote command
    //    goes through. The clone makes one store; the fetch, the push and the sync run on
    //    another, whose main is made from the fetched head.
    site.log_in(&[("github.com", ELSEWHERE), (&site.host(), TOKEN)]);
    let before = site.server.rev_parse(REPO, "main").expect("main");
    let mut runs = vec![site.run(&["--as", AGENT, "clone", &url, "--into", &into])];
    let line = [
        "--repo",
        into.as_str(),
        "remote",
        "add",
        "second",
        url.as_str(),
    ];
    runs.push(site.run(&line));
    assert_eq!(runs[1].ok()["branch"], "main", "{}", runs[1]);
    let store = site.store(&[]);
    runs.push(store.run_as(AGENT, &["fetch", ORIGIN], &[]));
    let fetched = runs[2].ok();
    let head = fetched["head"]["changeset"].as_str().expect("a head");
    let line = ["ref", "set", "main", head, "--create"];
    store.run_as(AGENT, &line, &[]).ok();
    land(&store, "local.txt", "local\n");
    runs.push(store.run_as(AGENT, &["push", ORIGIN], &[]));
    let pushed = site.server.rev_parse(REPO, "main").expect("main");
    assert_ne!(pushed, before, "the push moved the remote's main");
    land(&store, "more.txt", "more\n");
    runs.push(store.run_as(AGENT, &["sync", ORIGIN], &[]));
    for done in &runs {
        assert_eq!(done.exit(), 0, "{done}");
    }
    let synced = site.server.rev_parse(REPO, "main").expect("main");
    assert_ne!(synced, pushed, "the sync moved the remote's main");

    // 3. The fetches' requests and the pushes' carried the login's token, and no request
    //    carried anything else: not the other host's token.
    let header = Site::header(TOKEN);
    let requests = site.http.requests();
    for service in ["git-upload-pack", "git-receive-pack"] {
        let sent = requests.iter().any(|request| {
            request.path().ends_with(service) && request.header("Authorization") == Some(&header)
        });
        assert!(sent, "no {service} request with the login's token");
    }
    let sent = site.http.authorizations();
    assert!(sent.iter().all(|seen| *seen == header), "{sent:?}");

    // 4. The token is in no output and in no file of the store: the login file alone has it.
    for done in &runs {
        assert_unseen(done, TOKEN);
        assert_unseen(done, ELSEWHERE);
    }
    let stores = [Path::new(&into), Path::new(store.path())];
    for file in stores.into_iter().flat_map(files_under) {
        let bytes = fs::read(&file).expect("a readable file");
        for secret in [TOKEN.to_owned(), base64(&format!("{USER}:{TOKEN}"))] {
            let found = bytes.windows(secret.len()).any(|w| w == secret.as_bytes());
            assert!(!found, "{} holds the token", file.display());
        }
    }
    let remotes = fs::read_to_string(Path::new(&into).join("remotes.toml")).expect("remotes");
    assert!(!remotes.contains("token"), "{remotes}");

    // 5. The login removed: the remote refuses again, and the hint says where to look.
    site.log_in(&[("github.com", ELSEWHERE)]);
    let refused = store.run_as(AGENT, &["fetch", ORIGIN], &[]);
    let error = auth_refusal(&refused);
    assert!(error["details"].get("login_used").is_none(), "{refused}");
}

/// A login is for one host and port: logins for `github.com`, for the server's address at
/// another port, for the address without a port and for a name of it reach no request.
#[test]
fn a_login_for_another_port_is_not_sent() {
    let site = Site::new();
    let host = site.host();
    let (address, port) = host.rsplit_once(':').expect("a port");
    let name = format!("localhost:{port}");
    site.log_in(&[
        ("github.com", ELSEWHERE),
        (&site.other_port(), TOKEN),
        (address, TOKEN),
        (&name, TOKEN),
    ]);
    let store = site.store(&[]);

    let refused = store.run_as(AGENT, &["fetch", ORIGIN], &[]);
    let error = auth_refusal(&refused);
    assert!(error["details"].get("login_used").is_none(), "{refused}");
    assert!(!site.http.requests().is_empty(), "the remote was asked");
    assert_eq!(site.http.authorizations(), NO_HEADERS);
    assert_unseen(&refused, TOKEN);
}

/// A login whose token the remote refuses is the remote's refusal for the credential, and the
/// error says that the login's token was what it refused (`details.login_used`), without the
/// token.
#[test]
fn a_stale_login_is_an_auth_rejection() {
    let site = Site::new();
    site.log_in(&[(&site.host(), STALE)]);
    let store = site.store(&[]);

    let fetched = store.run_as(AGENT, &["fetch", ORIGIN], &[]);
    let synced = store.run_as(AGENT, &["sync", ORIGIN], &[]);
    for refused in [&fetched, &synced] {
        let error = auth_refusal(refused);
        assert_eq!(error["details"]["login_used"], true, "{refused}");
        assert!(
            error["details"].get("token_env_unset").is_none(),
            "{refused}"
        );
        assert_unseen(refused, STALE);
    }
    assert!(site.http.authorizations().contains(&Site::header(STALE)));
}

/// A login file that is there and is refused fails the command before git runs, whatever
/// the file holds: the remote is not asked without the token. A remote that names its variable
/// never opens the file, and goes on as before.
#[test]
fn a_loose_login_file_fails_the_fetch_closed() {
    let site = Site::new();
    site.log_in(&[(&site.host(), TOKEN)]);
    let file = site.file();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("set the mode");
    let store = site.store(&[]);
    let dir = tempfile::tempdir().expect("a temp dir");
    let into = path_text(&dir.path().join("clone"));
    let url = site.url();

    // 1. Each command that would reach the remote is refused for the file, and names it.
    let refusals = [
        store.run_as(AGENT, &["fetch", ORIGIN], &[]),
        store.run_as(AGENT, &["sync", ORIGIN], &[]),
        store.read(&["remote", "add", "second", &url]),
        site.run(&["--as", AGENT, "clone", &url, "--into", &into]),
    ];
    for refused in &refusals {
        let error = refused.err("E_INVALID_ARG");
        assert_eq!(error["details"]["reason"], "auth_file_mode", "{refused}");
        assert_eq!(error["details"]["auth_file"], path_text(&file), "{refused}");
        assert_unseen(refused, TOKEN);
    }
    assert!(site.http.requests().is_empty(), "nothing was sent");

    // 2. A remote that names its variable does not open the file.
    let named = site.store(&["--token-env", VAR]);
    named
        .run_as(AGENT, &["fetch", ORIGIN], &[(VAR, TOKEN)])
        .ok();

    // 3. The mode fixed as the hint says, the first store fetches with the login.
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("set the mode");
    store.run_as(AGENT, &["fetch", ORIGIN], &[]).ok();
}

/// A remote that names a token variable goes with that variable alone: with the variable
/// unset, the login for the same host is not sent in its place, and the error says the
/// variable was unset.
#[test]
fn a_named_variable_excludes_the_login() {
    let site = Site::new();
    site.log_in(&[(&site.host(), TOKEN)]);
    let store = site.store(&["--token-env", VAR]);

    let refused = store.run_as(AGENT, &["fetch", ORIGIN], &[]);
    let error = auth_refusal(&refused);
    assert_eq!(error["details"]["token_env_unset"], true, "{refused}");
    assert!(error["details"].get("login_used").is_none(), "{refused}");
    assert_eq!(site.http.authorizations(), NO_HEADERS);

    // A wrong token in the variable is refused as the variable's, not the login's.
    let refused = store.run_as(AGENT, &["fetch", ORIGIN], &[(VAR, STALE)]);
    let error = auth_refusal(&refused);
    assert!(error["details"].get("login_used").is_none(), "{refused}");
    assert_eq!(site.http.authorizations(), [Site::header(STALE)]);
}
